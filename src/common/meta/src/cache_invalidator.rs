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

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::error::Result;
use crate::instruction::CacheIdent;
use crate::key::table_info::TableInfoKey;
use crate::key::table_name::TableNameKey;
use crate::key::table_route::TableRouteKey;
use crate::key::TableMetaKey;

/// KvBackend cache invalidator
#[async_trait::async_trait]
pub trait KvCacheInvalidator: Send + Sync {
    async fn invalidate_key(&self, key: &[u8]);
}

pub type KvCacheInvalidatorRef = Arc<dyn KvCacheInvalidator>;

pub struct DummyKvCacheInvalidator;

#[async_trait::async_trait]
impl KvCacheInvalidator for DummyKvCacheInvalidator {
    async fn invalidate_key(&self, _key: &[u8]) {}
}

/// Places context of invalidating cache. e.g., span id, trace id etc.
#[derive(Default)]
pub struct Context {
    pub subject: Option<String>,
}

#[async_trait::async_trait]
pub trait CacheInvalidator: Send + Sync {
    async fn invalidate(&self, ctx: &Context, caches: Vec<CacheIdent>) -> Result<()>;
}

pub type CacheInvalidatorRef = Arc<dyn CacheInvalidator>;

pub struct DummyCacheInvalidator;

#[async_trait::async_trait]
impl CacheInvalidator for DummyCacheInvalidator {
    async fn invalidate(&self, _ctx: &Context, _caches: Vec<CacheIdent>) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MultiCacheInvalidator {
    invalidators: RwLock<Vec<CacheInvalidatorRef>>,
}

impl MultiCacheInvalidator {
    pub fn with_invalidators(invalidators: Vec<CacheInvalidatorRef>) -> Self {
        Self {
            invalidators: RwLock::new(invalidators),
        }
    }

    pub async fn add_invalidator(&self, invalidator: CacheInvalidatorRef) {
        self.invalidators.write().await.push(invalidator);
    }
}

#[async_trait::async_trait]
impl CacheInvalidator for MultiCacheInvalidator {
    async fn invalidate(&self, ctx: &Context, caches: Vec<CacheIdent>) -> Result<()> {
        let invalidators = self.invalidators.read().await;
        for invalidator in invalidators.iter() {
            invalidator.invalidate(ctx, caches.clone()).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T> CacheInvalidator for T
where
    T: KvCacheInvalidator,
{
    async fn invalidate(&self, _ctx: &Context, caches: Vec<CacheIdent>) -> Result<()> {
        for cache in caches {
            match cache {
                CacheIdent::TableId(table_id) => {
                    let key = TableInfoKey::new(table_id);
                    self.invalidate_key(&key.as_raw_key()).await;

                    let key = &TableRouteKey { table_id };
                    self.invalidate_key(&key.as_raw_key()).await;
                }
                CacheIdent::TableName(table_name) => {
                    let key: TableNameKey = (&table_name).into();
                    self.invalidate_key(&key.as_raw_key()).await
                }
            }
        }
        Ok(())
    }
}
