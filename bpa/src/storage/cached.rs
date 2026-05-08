//! LRU cache decorator for BundleStorage.

use core::num::NonZeroUsize;
use hardy_async::async_trait;
use hardy_async::sync::spin::Mutex;
use lru::LruCache;

use crate::{Arc, Bytes, stream::Sender};

use super::{BundleStorage, RecoveryResponse, Result};

/// Default LRU cache capacity (number of entries).
pub const DEFAULT_LRU_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// Default maximum bundle size (in bytes) eligible for caching.
pub const DEFAULT_MAX_CACHED_BUNDLE_SIZE: NonZeroUsize = NonZeroUsize::new(16 * 1024).unwrap();

/// Wraps a `BundleStorage` backend with an in-memory LRU cache.
///
/// Bundles smaller than `max_bundle_size` are cached on save/load.
/// The cache is transparent: callers use the standard `BundleStorage` trait.
pub struct CachedBundleStorage {
    inner: Arc<dyn BundleStorage>,
    lru: Mutex<LruCache<Arc<str>, Bytes>>,
    max_bundle_size: usize,
}

impl CachedBundleStorage {
    pub fn new(
        inner: Arc<dyn BundleStorage>,
        capacity: NonZeroUsize,
        max_bundle_size: NonZeroUsize,
    ) -> Self {
        Self {
            inner,
            lru: Mutex::new(LruCache::new(capacity)),
            max_bundle_size: max_bundle_size.into(),
        }
    }

    fn is_cacheable(&self, data: &[u8]) -> bool {
        data.len() < self.max_bundle_size
    }
}

#[async_trait]
impl BundleStorage for CachedBundleStorage {
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> Result<()> {
        self.inner.recover(stream).await
    }

    // SAFETY: load() is always the final storage access before delete().
    // Taking from the cache (rather than cloning) ensures the returned Bytes
    // has a single refcount, enabling in-place mutation via try_into_mut()
    // in the Editor's flatten_inplace() path.
    // On a cache miss, the inner backend (disk/sqlite) returns a fresh
    // Bytes with refcount=1, so we do not re-populate the cache.
    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>> {
        if let Some(data) = self.lru.lock().pop(storage_name) {
            metrics::counter!("bpa.store.cache.hits").increment(1);
            return Ok(Some(data));
        }

        metrics::counter!("bpa.store.cache.misses").increment(1);

        self.inner.load(storage_name).await
    }

    async fn save(&self, data: Bytes) -> Result<Arc<str>> {
        let storage_name = self.inner.save(data.clone()).await?;

        if self.is_cacheable(&data) {
            self.lru.lock().put(storage_name.clone(), data);
        } else {
            metrics::counter!("bpa.store.cache.oversized").increment(1);
        }

        Ok(storage_name)
    }

    async fn replace(&self, storage_name: &str, data: Bytes) -> Result<()> {
        self.inner.replace(storage_name, data.clone()).await?;

        if self.is_cacheable(&data) {
            self.lru.lock().put(storage_name.into(), data);
        } else {
            self.lru.lock().pop(storage_name);
        }

        Ok(())
    }

    async fn delete(&self, storage_name: &str) -> Result<()> {
        self.lru.lock().pop(storage_name);
        self.inner.delete(storage_name).await
    }
}
