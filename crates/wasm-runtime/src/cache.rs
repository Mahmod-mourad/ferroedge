//! Compiled WASM module cache.
//!
//! `wasmtime::Module` compilation is CPU-intensive. This cache keeps a bounded
//! number of compiled modules in memory with TTL-based expiry so they can be
//! reused across requests without recompiling from bytes each time.
//!
//! # Thread safety
//! `wasmtime::Module` is `Send + Sync` (wasmtime ≥1.0), so `Arc<Module>` is
//! safe to share across async tasks. All mutable state is guarded by a
//! `tokio::sync::Mutex` that is held only for brief, non-blocking operations;
//! actual compilation is done inside `spawn_blocking`.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::sync::Mutex;
use wasmtime::Module;

use crate::engine::WasmEngine;
use crate::sandbox::WasmError;

// ─── Public types ─────────────────────────────────────────────────────────────

/// An entry stored in the [`CompiledModuleCache`].
pub struct CachedModule {
    /// The compiled module, wrapped in `Arc` for cheap cloning.
    pub module: Arc<Module>,
    /// When this entry was inserted (or refreshed after TTL expiry).
    pub cached_at: Instant,
    /// Number of times this entry has been returned from cache.
    pub hit_count: u64,
    /// Size of the original WASM bytes that produced this module.
    pub wasm_bytes: usize,
}

/// Snapshot statistics returned by [`CompiledModuleCache::stats`].
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Current number of entries in the cache.
    pub size: usize,
    /// Total successful cache lookups.
    pub hits: u64,
    /// Total cache misses (compilations triggered).
    pub misses: u64,
    /// Total entries evicted (LRU overflow or TTL expiry).
    pub evictions: u64,
    /// Sum of original WASM byte sizes for all currently-cached modules.
    pub total_wasm_bytes: u64,
}

// ─── Internal state ───────────────────────────────────────────────────────────

struct CacheInner {
    lru: LruCache<String, CachedModule>,
    stats: CacheStats,
    /// Running sum of wasm_bytes for currently-cached entries.
    total_wasm_bytes: u64,
}

// ─── Enum used to avoid holding LRU borrow across stats update ────────────────

enum LookupResult {
    Hit(Arc<Module>),
    Expired,
    Miss,
}

// ─── Cache ────────────────────────────────────────────────────────────────────

/// LRU cache of compiled `wasmtime::Module` objects.
///
/// Create a single instance per process (e.g. wrapped in `Arc`) and share it
/// across request handlers.
pub struct CompiledModuleCache {
    engine: Arc<WasmEngine>,
    inner: Mutex<CacheInner>,
    ttl: Duration,
}

impl CompiledModuleCache {
    /// Create a new cache.
    ///
    /// * `max_size` — maximum number of compiled modules kept in memory.
    ///   When full, the least-recently-used entry is evicted.
    /// * `ttl_secs` — entries older than this are treated as expired and
    ///   recompiled on next access.
    pub fn new(engine: Arc<WasmEngine>, max_size: usize, ttl_secs: u64) -> Self {
        Self::new_with_ttl(engine, max_size, Duration::from_secs(ttl_secs))
    }

    /// Like [`new`] but accepts an arbitrary [`Duration`] for the TTL.
    /// Useful in tests that need sub-second TTLs.
    pub fn new_with_ttl(engine: Arc<WasmEngine>, max_size: usize, ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(max_size.max(1)).expect("max_size must be > 0");
        Self {
            engine,
            inner: Mutex::new(CacheInner {
                lru: LruCache::new(cap),
                stats: CacheStats::default(),
                total_wasm_bytes: 0,
            }),
            ttl,
        }
    }

    /// Return a cached module for `key` if one exists and has not expired.
    ///
    /// Unlike [`get_or_compile`], this never triggers compilation. Updates
    /// hit-count on a cache hit but does NOT increment miss stats.
    pub async fn get_cached(&self, key: &str) -> Option<Arc<Module>> {
        let mut inner = self.inner.lock().await;
        let ttl = self.ttl;

        // Probe the LRU.  We use a local enum to carry the result out of the
        // `get_mut` borrow so that we can update `inner.stats` separately —
        // the borrow checker cannot see that `inner.lru` and `inner.stats`
        // are disjoint fields when accessed through a `MutexGuard`.
        let result = match inner.lru.get_mut(key) {
            None => LookupResult::Miss,
            Some(entry) => {
                if entry.cached_at.elapsed() < ttl {
                    entry.hit_count += 1;
                    LookupResult::Hit(Arc::clone(&entry.module))
                } else {
                    LookupResult::Expired
                }
            }
        };
        // LRU borrow ends here.

        match result {
            LookupResult::Hit(module) => {
                inner.stats.hits += 1;
                Some(module)
            }
            LookupResult::Expired => {
                if let Some(evicted) = inner.lru.pop(key) {
                    inner.total_wasm_bytes =
                        inner.total_wasm_bytes.saturating_sub(evicted.wasm_bytes as u64);
                }
                inner.stats.evictions += 1;
                None
            }
            LookupResult::Miss => None,
        }
    }

    /// Return the cached module for `key`, compiling from `wasm_bytes` if
    /// the key is absent or its TTL has expired.
    ///
    /// Compilation is performed inside `tokio::task::spawn_blocking` so this
    /// async function never blocks the executor thread.
    pub async fn get_or_compile(
        &self,
        key: &str,
        wasm_bytes: &[u8],
    ) -> Result<Arc<Module>, WasmError> {
        // ── 1. Fast path: valid cached entry ──────────────────────────────────
        {
            let mut inner = self.inner.lock().await;
            let ttl = self.ttl;

            let result = match inner.lru.get_mut(key) {
                None => LookupResult::Miss,
                Some(entry) => {
                    if entry.cached_at.elapsed() < ttl {
                        entry.hit_count += 1;
                        LookupResult::Hit(Arc::clone(&entry.module))
                    } else {
                        LookupResult::Expired
                    }
                }
            };
            // LRU borrow ends here.

            match result {
                LookupResult::Hit(module) => {
                    inner.stats.hits += 1;
                    return Ok(module);
                }
                LookupResult::Expired => {
                    if let Some(evicted) = inner.lru.pop(key) {
                        inner.total_wasm_bytes =
                            inner.total_wasm_bytes.saturating_sub(evicted.wasm_bytes as u64);
                    }
                    inner.stats.evictions += 1;
                }
                LookupResult::Miss => {}
            }

            inner.stats.misses += 1;
        }
        // Mutex released before blocking work.

        // ── 2. Compile in a blocking thread ───────────────────────────────────
        let engine_clone = Arc::clone(&self.engine);
        let bytes = wasm_bytes.to_vec();
        let module = tokio::task::spawn_blocking(move || {
            wasmtime::Module::new(&engine_clone.inner, &bytes)
                .map_err(|e| WasmError::CompilationFailed(e.to_string()))
        })
        .await
        .map_err(|e| WasmError::CompilationFailed(format!("spawn_blocking panicked: {e}")))?
        .map(Arc::new)?;

        // ── 3. Insert into cache ───────────────────────────────────────────────
        {
            let mut inner = self.inner.lock().await;
            let at_cap = inner.lru.len() == inner.lru.cap().get();
            // If LRU will evict, subtract the evicted entry's bytes first.
            if at_cap {
                if let Some((_, evicted)) = inner.lru.peek_lru() {
                    inner.total_wasm_bytes =
                        inner.total_wasm_bytes.saturating_sub(evicted.wasm_bytes as u64);
                }
                inner.stats.evictions += 1;
            }
            let wasm_size = wasm_bytes.len();
            inner.lru.put(
                key.to_string(),
                CachedModule {
                    module: Arc::clone(&module),
                    cached_at: Instant::now(),
                    hit_count: 0,
                    wasm_bytes: wasm_size,
                },
            );
            inner.total_wasm_bytes += wasm_size as u64;
        }

        Ok(module)
    }

    /// Remove the entry for `key` from the cache (no-op if absent).
    pub async fn invalidate(&self, key: &str) {
        let mut inner = self.inner.lock().await;
        inner.lru.pop(key);
    }

    /// Return a point-in-time snapshot of cache statistics.
    pub async fn stats(&self) -> CacheStats {
        let inner = self.inner.lock().await;
        CacheStats {
            size: inner.lru.len(),
            total_wasm_bytes: inner.total_wasm_bytes,
            ..inner.stats.clone()
        }
    }
}
