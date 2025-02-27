// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![warn(unused_imports)]

use datatypes::data_type::ConcreteDataType as CDT;
use itertools::Itertools;
use snafu::{OptionExt, ResultExt};
use substrait::substrait_proto::proto::expression::field_reference::ReferenceType::DirectReference;
use substrait::substrait_proto::proto::expression::reference_segment::ReferenceType::StructField;
use substrait::substrait_proto::proto::expression::{IfThen, RexType, ScalarFunction};
use substrait::substrait_proto::proto::function_argument::ArgType;
use substrait::substrait_proto::proto::Expression;

use crate::adapter::error::{
    DatatypesSnafu, Error, EvalSnafu, InvalidQuerySnafu, NotImplementedSnafu, PlanSnafu,
};
use crate::expr::{
    BinaryFunc, ScalarExpr, TypedExpr, UnaryFunc, UnmaterializableFunc, VariadicFunc,
};
use crate::repr::{ColumnType, RelationType};
use crate::transform::literal::{from_substrait_literal, from_substrait_type};
use crate::transform::FunctionExtensions;

impl TypedExpr {
    /// Convert ScalarFunction into Flow's ScalarExpr
    pub fn from_substrait_scalar_func(
        f: &ScalarFunction,
        input_schema: &RelationType,
        extensions: &FunctionExtensions,
    ) -> Result<TypedExpr, Error> {
        let fn_name =
            extensions
                .get(&f.function_reference)
                .with_context(|| NotImplementedSnafu {
                    reason: format!(
                        "Aggregated function not found: function reference = {:?}",
                        f.function_reference
                    ),
                })?;
        let arg_len = f.arguments.len();
        let arg_exprs: Vec<TypedExpr> = f
            .arguments
            .iter()
            .map(|arg| match &arg.arg_type {
                Some(ArgType::Value(e)) => {
                    TypedExpr::from_substrait_rex(e, input_schema, extensions)
                }
                _ => not_impl_err!("Aggregated function argument non-Value type not supported"),
            })
            .try_collect()?;

        // literal's type is determined by the function and type of other args
        let (arg_exprs, arg_types): (Vec<_>, Vec<_>) = arg_exprs
            .into_iter()
            .map(
                |TypedExpr {
                     expr: arg_val,
                     typ: arg_type,
                 }| {
                    if arg_val.is_literal() {
                        (arg_val, None)
                    } else {
                        (arg_val, Some(arg_type.scalar_type))
                    }
                },
            )
            .unzip();

        match arg_len {
            // because variadic function can also have 1 arguments, we need to check if it's a variadic function first
            1 if VariadicFunc::from_str_and_types(fn_name, &arg_types).is_err() => {
                let func = UnaryFunc::from_str_and_type(fn_name, None)?;
                let arg = arg_exprs[0].clone();
                let ret_type = ColumnType::new_nullable(func.signature().output.clone());

                Ok(TypedExpr::new(arg.call_unary(func), ret_type))
            }
            // because variadic function can also have 2 arguments, we need to check if it's a variadic function first
            2 if VariadicFunc::from_str_and_types(fn_name, &arg_types).is_err() => {
                let (func, signature) =
                    BinaryFunc::from_str_expr_and_type(fn_name, &arg_exprs, &arg_types[0..2])?;

                // constant folding here
                let is_all_literal = arg_exprs.iter().all(|arg| arg.is_literal());
                if is_all_literal {
                    let res = func
                        .eval(&[], &arg_exprs[0], &arg_exprs[1])
                        .context(EvalSnafu)?;

                    // if output type is null, it should be inferred from the input types
                    let con_typ = signature.output.clone();
                    let typ = ColumnType::new_nullable(con_typ.clone());
                    return Ok(TypedExpr::new(ScalarExpr::Literal(res, con_typ), typ));
                }

                let mut arg_exprs = arg_exprs;
                for (idx, arg_expr) in arg_exprs.iter_mut().enumerate() {
                    if let ScalarExpr::Literal(val, typ) = arg_expr {
                        let dest_type = signature.input[idx].clone();

                        // cast val to target_type
                        let dest_val = if !dest_type.is_null() {
                            datatypes::types::cast(val.clone(), &dest_type)
                        .with_context(|_|
                            DatatypesSnafu{
                                extra: format!("Failed to implicitly cast literal {val:?} to type {dest_type:?}")
                            })?
                        } else {
                            val.clone()
                        };
                        *val = dest_val;
                        *typ = dest_type;
                    }
                }

                let ret_type = ColumnType::new_nullable(func.signature().output.clone());
                let ret_expr = arg_exprs[0].clone().call_binary(arg_exprs[1].clone(), func);
                Ok(TypedExpr::new(ret_expr, ret_type))
            }
            _var => {
                if let Ok(func) = VariadicFunc::from_str_and_types(fn_name, &arg_types) {
                    let ret_type = ColumnType::new_nullable(func.signature().output.clone());
                    let mut expr = ScalarExpr::CallVariadic {
                        func,
                        exprs: arg_exprs,
                    };
                    expr.optimize();
                    Ok(TypedExpr::new(expr, ret_type))
                } else if let Ok(func) = UnmaterializableFunc::from_str(fn_name) {
                    let ret_type = ColumnType::new_nullable(func.signature().output.clone());
                    Ok(TypedExpr::new(
                        ScalarExpr::CallUnmaterializable(func),
                        ret_type,
                    ))
                } else {
                    not_impl_err!("Unsupported function {fn_name} with {arg_len} arguments")
                }
            }
        }
    }

    /// Convert IfThen into Flow's ScalarExpr
    pub fn from_substrait_ifthen_rex(
        if_then: &IfThen,
        input_schema: &RelationType,
        extensions: &FunctionExtensions,
    ) -> Result<TypedExpr, Error> {
        let ifs: Vec<_> = if_then
            .ifs
            .iter()
            .map(|if_clause| {
                let proto_if = if_clause.r#if.as_ref().with_context(|| InvalidQuerySnafu {
                    reason: "IfThen clause without if",
                })?;
                let proto_then = if_clause.then.as_ref().with_context(|| InvalidQuerySnafu {
                    reason: "IfThen clause without then",
                })?;
                let cond = TypedExpr::from_substrait_rex(proto_if, input_schema, extensions)?;
                let then = TypedExpr::from_substrait_rex(proto_then, input_schema, extensions)?;
                Ok((cond, then))
            })
            .try_collect()?;
        // if no else is presented
        let els = if_then
            .r#else
            .as_ref()
            .map(|e| TypedExpr::from_substrait_rex(e, input_schema, extensions))
            .transpose()?
            .unwrap_or_else(|| {
                TypedExpr::new(
                    ScalarExpr::literal_null(),
                    ColumnType::new_nullable(CDT::null_datatype()),
                )
            });

        fn build_if_then_recur(
            mut next_if_then: impl Iterator<Item = (TypedExpr, TypedExpr)>,
            els: TypedExpr,
        ) -> TypedExpr {
            if let Some((cond, then)) = next_if_then.next() {
                // always assume the type of `if`` expr is the same with the `then`` expr
                TypedExpr::new(
                    ScalarExpr::If {
                        cond: Box::new(cond.expr),
                        then: Box::new(then.expr),
                        els: Box::new(build_if_then_recur(next_if_then, els).expr),
                    },
                    then.typ,
                )
            } else {
                els
            }
        }
        let expr_if = build_if_then_recur(ifs.into_iter(), els);
        Ok(expr_if)
    }
    /// Convert Substrait Rex into Flow's ScalarExpr
    pub fn from_substrait_rex(
        e: &Expression,
        input_schema: &RelationType,
        extensions: &FunctionExtensions,
    ) -> Result<TypedExpr, Error> {
        match &e.rex_type {
            Some(RexType::Literal(lit)) => {
                let lit = from_substrait_literal(lit)?;
                Ok(TypedExpr::new(
                    ScalarExpr::Literal(lit.0, lit.1.clone()),
                    ColumnType::new_nullable(lit.1),
                ))
            }
            Some(RexType::SingularOrList(s)) => {
                let substrait_expr = s.value.as_ref().with_context(|| InvalidQuerySnafu {
                    reason: "SingularOrList expression without value",
                })?;
                // Note that we didn't impl support to in list expr
                if !s.options.is_empty() {
                    return not_impl_err!("In list expression is not supported");
                }
                TypedExpr::from_substrait_rex(substrait_expr, input_schema, extensions)
            }
            Some(RexType::Selection(field_ref)) => match &field_ref.reference_type {
                Some(DirectReference(direct)) => match &direct.reference_type.as_ref() {
                    Some(StructField(x)) => match &x.child.as_ref() {
                        Some(_) => {
                            not_impl_err!(
                                "Direct reference StructField with child is not supported"
                            )
                        }
                        None => {
                            let column = x.field as usize;
                            let column_type = input_schema.column_types[column].clone();
                            Ok(TypedExpr::new(ScalarExpr::Column(column), column_type))
                        }
                    },
                    _ => not_impl_err!(
                        "Direct reference with types other than StructField is not supported"
                    ),
                },
                _ => not_impl_err!("unsupported field ref type"),
            },
            Some(RexType::ScalarFunction(f)) => {
                TypedExpr::from_substrait_scalar_func(f, input_schema, extensions)
            }
            Some(RexType::IfThen(if_then)) => {
                TypedExpr::from_substrait_ifthen_rex(if_then, input_schema, extensions)
            }
            Some(RexType::Cast(cast)) => {
                let input = cast.input.as_ref().with_context(|| InvalidQuerySnafu {
                    reason: "Cast expression without input",
                })?;
                let input = TypedExpr::from_substrait_rex(input, input_schema, extensions)?;
                let cast_type = from_substrait_type(cast.r#type.as_ref().with_context(|| {
                    InvalidQuerySnafu {
                        reason: "Cast expression without type",
                    }
                })?)?;
                let func = UnaryFunc::from_str_and_type("cast", Some(cast_type.clone()))?;
                Ok(TypedExpr::new(
                    input.expr.call_unary(func),
                    ColumnType::new_nullable(cast_type),
                ))
            }
            Some(RexType::WindowFunction(_)) => PlanSnafu {
                reason:
                    "Window function is not supported yet. Please use aggregation function instead."
                        .to_string(),
            }
            .fail(),
            _ => not_impl_err!("unsupported rex_type"),
        }
    }
}

#[cfg(test)]
mod test {
    use datatypes::value::Value;

    use super::*;
    use crate::expr::{GlobalId, MapFilterProject};
    use crate::plan::{Plan, TypedPlan};
    use crate::repr::{self, ColumnType, RelationType};
    use crate::transform::test::{create_test_ctx, create_test_query_engine, sql_to_substrait};
    /// test if `WHERE` condition can be converted to Flow's ScalarExpr in mfp's filter
    #[tokio::test]
    async fn test_where_and() {
        let engine = create_test_query_engine();
        let sql = "SELECT number FROM numbers WHERE number >= 1 AND number <= 3 AND number!=2";
        let plan = sql_to_substrait(engine.clone(), sql).await;

        let mut ctx = create_test_ctx();
        let flow_plan = TypedPlan::from_substrait_plan(&mut ctx, &plan);

        // optimize binary and to variadic and
        let filter = ScalarExpr::CallVariadic {
            func: VariadicFunc::And,
            exprs: vec![
                ScalarExpr::Column(0).call_binary(
                    ScalarExpr::Literal(Value::from(1u32), CDT::uint32_datatype()),
                    BinaryFunc::Gte,
                ),
                ScalarExpr::Column(0).call_binary(
                    ScalarExpr::Literal(Value::from(3u32), CDT::uint32_datatype()),
                    BinaryFunc::Lte,
                ),
                ScalarExpr::Column(0).call_binary(
                    ScalarExpr::Literal(Value::from(2u32), CDT::uint32_datatype()),
                    BinaryFunc::NotEq,
                ),
            ],
        };
        let expected = TypedPlan {
            typ: RelationType::new(vec![ColumnType::new(CDT::uint32_datatype(), false)]),
            plan: Plan::Mfp {
                input: Box::new(Plan::Get {
                    id: crate::expr::Id::Global(GlobalId::User(0)),
                }),
                mfp: MapFilterProject::new(1)
                    .map(vec![ScalarExpr::Column(0)])
                    .unwrap()
                    .filter(vec![filter])
                    .unwrap()
                    .project(vec![1])
                    .unwrap(),
            },
        };
        assert_eq!(flow_plan.unwrap(), expected);
    }

    /// case: binary functions&constant folding can happen in converting substrait plan
    #[tokio::test]
    async fn test_binary_func_and_constant_folding() {
        let engine = create_test_query_engine();
        let sql = "SELECT 1+1*2-1/1+1%2==3 FROM numbers";
        let plan = sql_to_substrait(engine.clone(), sql).await;

        let mut ctx = create_test_ctx();
        let flow_plan = TypedPlan::from_substrait_plan(&mut ctx, &plan);

        let expected = TypedPlan {
            typ: RelationType::new(vec![ColumnType::new(CDT::boolean_datatype(), true)]),
            plan: Plan::Constant {
                rows: vec![(
                    repr::Row::new(vec![Value::from(true)]),
                    repr::Timestamp::MIN,
                    1,
                )],
            },
        };

        assert_eq!(flow_plan.unwrap(), expected);
    }

    /// test if the type of the literal is correctly inferred, i.e. in here literal is decoded to be int64, but need to be uint32,
    #[tokio::test]
    async fn test_implicitly_cast() {
        let engine = create_test_query_engine();
        let sql = "SELECT number+1 FROM numbers";
        let plan = sql_to_substrait(engine.clone(), sql).await;

        let mut ctx = create_test_ctx();
        let flow_plan = TypedPlan::from_substrait_plan(&mut ctx, &plan);

        let expected = TypedPlan {
            typ: RelationType::new(vec![ColumnType::new(CDT::uint32_datatype(), true)]),
            plan: Plan::Mfp {
                input: Box::new(Plan::Get {
                    id: crate::expr::Id::Global(GlobalId::User(0)),
                }),
                mfp: MapFilterProject::new(1)
                    .map(vec![ScalarExpr::Column(0).call_binary(
                        ScalarExpr::Literal(Value::from(1u32), CDT::uint32_datatype()),
                        BinaryFunc::AddUInt32,
                    )])
                    .unwrap()
                    .project(vec![1])
                    .unwrap(),
            },
        };
        assert_eq!(flow_plan.unwrap(), expected);
    }

    #[tokio::test]
    async fn test_cast() {
        let engine = create_test_query_engine();
        let sql = "SELECT CAST(1 AS INT16) FROM numbers";
        let plan = sql_to_substrait(engine.clone(), sql).await;

        let mut ctx = create_test_ctx();
        let flow_plan = TypedPlan::from_substrait_plan(&mut ctx, &plan);

        let expected = TypedPlan {
            typ: RelationType::new(vec![ColumnType::new(CDT::int16_datatype(), true)]),
            plan: Plan::Mfp {
                input: Box::new(Plan::Get {
                    id: crate::expr::Id::Global(GlobalId::User(0)),
                }),
                mfp: MapFilterProject::new(1)
                    .map(vec![ScalarExpr::Literal(
                        Value::Int64(1),
                        CDT::int64_datatype(),
                    )
                    .call_unary(UnaryFunc::Cast(CDT::int16_datatype()))])
                    .unwrap()
                    .project(vec![1])
                    .unwrap(),
            },
        };
        assert_eq!(flow_plan.unwrap(), expected);
    }

    #[tokio::test]
    async fn test_select_add() {
        let engine = create_test_query_engine();
        let sql = "SELECT number+number FROM numbers";
        let plan = sql_to_substrait(engine.clone(), sql).await;

        let mut ctx = create_test_ctx();
        let flow_plan = TypedPlan::from_substrait_plan(&mut ctx, &plan);

        let expected = TypedPlan {
            typ: RelationType::new(vec![ColumnType::new(CDT::uint32_datatype(), true)]),
            plan: Plan::Mfp {
                input: Box::new(Plan::Get {
                    id: crate::expr::Id::Global(GlobalId::User(0)),
                }),
                mfp: MapFilterProject::new(1)
                    .map(vec![ScalarExpr::Column(0)
                        .call_binary(ScalarExpr::Column(0), BinaryFunc::AddUInt32)])
                    .unwrap()
                    .project(vec![1])
                    .unwrap(),
            },
        };

        assert_eq!(flow_plan.unwrap(), expected);
    }
}
