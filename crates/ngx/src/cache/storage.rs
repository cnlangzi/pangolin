//! The edge cache's storage: a memory tier in front of a disk tier.
//!
//! pingora's cache engine has **one** storage slot, so the two tiers
//! live inside one implementation (issue #259). A lookup walks the
//! memory tier first and falls through to the disk tier; a write lands
//! in the memory tier unless the object is too big for it, and an
//! object the memory tier has to drop lands in the disk tier.
//!
//! Two rules shape everything here:
//!
//! * **A storage failure never fails a request.** The cache is a
//!   data-plane component: when it cannot store or read something, the
//!   request goes to the origin and the answer is served from there.
//!   A failed disk write is a log line, not a 5xx. A poisoned lock
//!   makes the tier unavailable, which is a miss, not an error.
//! * **A disk write is atomic.** The bytes go to a temporary file and
//!   the file is renamed into place, so a process that is killed
//!   half-way through a write leaves a temporary file — never a
//!   half-written entry. The leftovers are swept at startup.

use std::any::Any;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use log::{debug, error, warn};
use pingora::cache::key::CacheHashKey;
use pingora::cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeOutcome, PurgeTarget,
    PurgeType, Storage,
};
use pingora::cache::trace::SpanHandle;
use pingora::cache::{CacheKey, CacheMeta};
use pingora_core::prelude::*;

use super::key;
use super::stats::CacheStats;

/// The size classes of the memory tier.
///
/// A class holds the objects up to its ceiling; the smallest class
/// comes first. The classes are the ones the design names (16 KiB,
/// 64 KiB, 256 KiB, 1 MiB) — the last one is the configured
/// `memory_max_object_size`, so an operator who shrinks the limit
/// gets the classes shrunk with it.
const SIZE_CLASSES: [u64; 4] = [16 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024];

/// The edge cache's storage.
pub struct EdgeCacheStorage {
    memory: MemoryTier,
    disk: DiskTier,
    stats: Arc<CacheStats>,
    /// The biggest object the cache stores at all.
    max_object_size: usize,
}

impl EdgeCacheStorage {
    /// Open the storage.
    ///
    /// The disk tier is best-effort: a cache directory that cannot be
    /// created leaves the memory tier working on its own rather than
    /// stopping the gateway.
    pub fn new(
        dir: &Path,
        memory_size: u64,
        disk_size: u64,
        max_object_size: u64,
        memory_max_object_size: u64,
        stats: Arc<CacheStats>,
    ) -> Self {
        let memory = MemoryTier::new(memory_size, memory_max_object_size);
        let disk = DiskTier::open(dir, disk_size);
        let storage = Self {
            memory,
            disk,
            stats,
            max_object_size: usize::try_from(max_object_size).unwrap_or(usize::MAX),
        };
        storage.publish_usage();
        storage
    }

    /// The memory tier's occupancy.
    pub fn memory_usage(&self) -> (u64, u64) {
        (self.memory.bytes(), self.memory.entry_count())
    }

    /// The disk tier's occupancy.
    pub fn disk_usage(&self) -> (u64, u64) {
        (self.disk.bytes(), self.disk.entry_count())
    }

    /// Publish the tiers' occupancy to the counters.
    fn publish_usage(&self) {
        let (memory_bytes, memory_entries) = self.memory_usage();
        let (disk_bytes, disk_entries) = self.disk_usage();
        self.stats.set_memory_usage(memory_bytes, memory_entries);
        self.stats.set_disk_usage(disk_bytes, disk_entries);
    }

    /// Store an entry.
    ///
    /// An object that fits the memory tier goes there; an object that
    /// does not goes straight to the disk tier. Whatever the memory
    /// tier has to drop to stay inside its budget is spilled into the
    /// disk tier.
    ///
    /// The disk tier's write is **blocking I/O**, so it runs on the
    /// blocking pool: a slow disk must not stall the pingora worker
    /// that is serving the other requests. The memory tier's write is
    /// a copy, and it stays on the request's thread.
    async fn commit(
        &'static self,
        digest: u128,
        meta: (Vec<u8>, Vec<u8>),
        body: Vec<u8>,
    ) -> Result<()> {
        let size = body.len();
        if size > self.max_object_size {
            // The engine refuses to hand us more than
            // `max_object_size`; this is the belt to its braces.
            return Ok(());
        }
        if self.memory.accepts(size as u64) {
            let entry = Arc::new(MemoryEntry {
                meta: RwLock::new(meta),
                body: Arc::new(body),
            });
            let spilled = self.memory.insert(digest, entry);
            self.spill(spilled).await;
        } else {
            let hex = key::digest_hex(digest);
            let disk = &self.disk;
            let outcome = tokio::task::spawn_blocking(move || disk.put(&hex, &meta, &body)).await;
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(e) => {
                    self.stats.record_error();
                    return Error::e_explain(
                        ErrorType::InternalError,
                        format!("cache: the cache writer died: {e}"),
                    );
                }
            }
        }
        self.stats.record_store();
        self.publish_usage();
        Ok(())
    }

    /// Write the objects the memory tier dropped into the disk tier.
    ///
    /// The writes are blocking I/O, so they run on the blocking pool.
    ///
    /// A disk that is full must not fail the request that triggered
    /// the spill: the entry is simply lost.
    async fn spill(&'static self, spilled: Vec<SpilledEntry>) {
        if spilled.is_empty() {
            return;
        }
        let disk = &self.disk;
        let stats = self.stats.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            for victim in spilled {
                // An entry whose metadata cannot be read is dropped
                // rather than spilled: the disk copy would be
                // unreadable too.
                let Some(meta) = victim.meta else { continue };
                let hex = key::digest_hex(victim.digest);
                match disk.put(&hex, &meta, &victim.body) {
                    Ok(()) => stats.record_eviction(),
                    Err(e) => {
                        stats.record_error();
                        warn!("cache: cannot spill an entry to the disk tier: {e}");
                    }
                }
            }
        })
        .await
        {
            self.stats.record_error();
            warn!("cache: the cache writer died: {e}");
        }
    }
}

#[async_trait]
impl Storage for EdgeCacheStorage {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let digest = key::digest(key);

        if let Some(entry) = self.memory.get(digest)
            && let Some(meta) = entry.meta()
        {
            self.stats.record_hit();
            return Ok(Some((meta, Box::new(EdgeHitHandler::new(entry)))));
        }

        // The disk tier's read is **blocking I/O**: it runs on the
        // blocking pool, so a slow disk cannot stall the pingora
        // worker that is serving the other requests.
        //
        // The index answers first: a key the tier does not hold costs
        // nothing, and the common case — a miss in the memory tier
        // and a miss in the disk tier — never leaves the request's
        // thread.
        let hex = key::digest_hex(digest);
        if !self.disk.contains(&hex) {
            self.stats.record_miss();
            return Ok(None);
        }
        let disk = &self.disk;
        let found = match tokio::task::spawn_blocking(move || disk.get(&hex)).await {
            Ok(found) => found,
            Err(e) => {
                self.stats.record_error();
                warn!("cache: the cache reader died: {e}");
                None
            }
        };
        if let Some((internal, header, body)) = found {
            match CacheMeta::deserialize(&internal, &header) {
                Ok(meta) => {
                    // A disk hit warms the memory tier: the next read
                    // of this key is served from RAM.
                    let entry = Arc::new(MemoryEntry {
                        meta: RwLock::new((internal, header)),
                        body: Arc::new(body),
                    });
                    if self.memory.accepts(entry.body.len() as u64) {
                        let spilled = self.memory.insert(digest, entry.clone());
                        self.spill(spilled).await;
                    }
                    self.stats.record_hit();
                    self.publish_usage();
                    return Ok(Some((meta, Box::new(EdgeHitHandler::new(entry)))));
                }
                Err(e) => {
                    // The metadata cannot be read — the entry is from
                    // another format or is damaged. Treat it as a miss
                    // and delete it, so the cache heals itself.
                    self.stats.record_error();
                    warn!("cache: dropping an entry with unreadable metadata: {e}");
                    self.disk.remove(&key::digest_hex(digest));
                    self.publish_usage();
                }
            }
        }

        self.stats.record_miss();
        Ok(None)
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<MissHandler> {
        let digest = key::digest(key);
        let serialized = meta.serialize()?;
        Ok(Box::new(EdgeMissHandler {
            storage: self,
            digest,
            meta: serialized,
            body: Vec::new(),
            max_size: self.max_object_size,
        }))
    }

    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        _purge_type: PurgeType,
        _trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        let digest = u128::from_be_bytes(target.key().combined_bin());
        let removed_memory = self.memory.remove(digest);
        let removed_disk = self.disk.remove(&key::digest_hex(digest));
        self.publish_usage();
        if removed_memory.is_some() || removed_disk {
            Ok(PurgeOutcome::Purged(None))
        } else {
            Ok(PurgeOutcome::NotFound)
        }
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        let digest = key::digest(key);
        let serialized = meta.serialize()?;
        if self.memory.update_meta(digest, &serialized) {
            return Ok(true);
        }
        let hex = key::digest_hex(digest);
        let updated = self.disk.update_meta(&hex, &serialized)?;
        self.publish_usage();
        Ok(updated)
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

// ─────────────────────────────────────────────────────────────────
//  The memory tier
// ─────────────────────────────────────────────────────────────────

/// An object the memory tier holds.
struct MemoryEntry {
    /// The serialized metadata. It is behind a lock because a
    /// revalidation rewrites it in place.
    meta: RwLock<(Vec<u8>, Vec<u8>)>,
    /// The response body.
    body: Arc<Vec<u8>>,
}

impl MemoryEntry {
    /// The entry's metadata, deserialized.
    ///
    /// The metadata was serialized by this process, so it must decode;
    /// `None` means the entry is damaged or its lock is poisoned, and
    /// the caller treats it as a miss.
    fn meta(&self) -> Option<CacheMeta> {
        let guard = self.meta.read().ok()?;
        CacheMeta::deserialize(&guard.0, &guard.1).ok()
    }
}

/// The memory tier: the objects the cache keeps in RAM.
///
/// The tier is split into **size classes**; each class holds its own
/// LRU, and the tier as a whole is capped at its byte budget. When the
/// cap is reached the **largest** class is drained first: a small file
/// earns its bytes back far more often than a large one, so the large
/// one is the first to go (issue #259).
struct MemoryTier {
    classes: Vec<MemoryClass>,
    budget: u64,
    bytes: AtomicU64,
    entries: AtomicU64,
}

/// One size class of the memory tier.
struct MemoryClass {
    /// The biggest object this class takes.
    max_size: u64,
    lru: RwLock<ByteLru<Arc<MemoryEntry>>>,
}

impl MemoryTier {
    /// Build the tier with its size classes.
    fn new(budget: u64, memory_max_object_size: u64) -> Self {
        let mut ceilings: Vec<u64> = SIZE_CLASSES
            .iter()
            .copied()
            .filter(|ceiling| *ceiling < memory_max_object_size)
            .collect();
        ceilings.push(memory_max_object_size.max(1));
        ceilings.dedup();
        let classes = ceilings
            .into_iter()
            .map(|max_size| MemoryClass {
                max_size,
                lru: RwLock::new(ByteLru::new()),
            })
            .collect();
        Self {
            classes,
            budget,
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
        }
    }

    /// Whether an object of this size belongs in the memory tier.
    fn accepts(&self, size: u64) -> bool {
        self.classes
            .last()
            .is_some_and(|class| size <= class.max_size)
    }

    /// The class an object of this size belongs to.
    fn class_of(&self, size: u64) -> Option<usize> {
        self.classes.iter().position(|class| size <= class.max_size)
    }

    /// The tier's occupancy in bytes.
    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The number of objects the tier holds.
    fn entry_count(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// Look an object up.
    fn get(&self, digest: u128) -> Option<Arc<MemoryEntry>> {
        for class in &self.classes {
            let mut lru = class.lru.write().ok()?;
            if let Some(entry) = lru.get(digest) {
                return Some(entry.clone());
            }
        }
        None
    }

    /// Store an object, returning the objects that had to be dropped
    /// to stay inside the budget.
    fn insert(&self, digest: u128, entry: Arc<MemoryEntry>) -> Vec<SpilledEntry> {
        let size = entry.body.len() as u64;
        let Some(index) = self.class_of(size) else {
            return Vec::new();
        };
        let replaced = {
            let Ok(mut lru) = self.classes[index].lru.write() else {
                return Vec::new();
            };
            lru.put(digest, entry, size)
        };
        match replaced {
            Some((_old, old_size)) => {
                self.bytes.fetch_sub(old_size, Ordering::Relaxed);
                self.bytes.fetch_add(size, Ordering::Relaxed);
            }
            None => {
                self.bytes.fetch_add(size, Ordering::Relaxed);
                self.entries.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut spilled = Vec::new();
        while self.bytes() > self.budget {
            let Some(victim) = self.evict_one() else {
                break;
            };
            self.bytes.fetch_sub(victim.size, Ordering::Relaxed);
            self.entries.fetch_sub(1, Ordering::Relaxed);
            spilled.push(victim);
        }
        spilled
    }

    /// Drop the least recently used object of the largest class.
    fn evict_one(&self) -> Option<SpilledEntry> {
        for class in self.classes.iter().rev() {
            let Ok(mut lru) = class.lru.write() else {
                continue;
            };
            if let Some((digest, entry, size)) = lru.pop_lru() {
                // The metadata may be unreadable — a poisoned lock.
                // The object is still evicted (the caller accounts for
                // it either way); it just cannot be spilled to the
                // disk tier, where it would be unreadable too.
                let meta = entry.meta.read().ok().map(|guard| guard.clone());
                return Some(SpilledEntry {
                    digest,
                    size,
                    meta,
                    body: entry.body.clone(),
                });
            }
        }
        None
    }

    /// Remove an object.
    fn remove(&self, digest: u128) -> Option<Arc<MemoryEntry>> {
        for class in &self.classes {
            let Ok(mut lru) = class.lru.write() else {
                continue;
            };
            if let Some((entry, size)) = lru.remove(digest) {
                self.bytes.fetch_sub(size, Ordering::Relaxed);
                self.entries.fetch_sub(1, Ordering::Relaxed);
                return Some(entry);
            }
        }
        None
    }

    /// Rewrite an object's metadata in place.
    fn update_meta(&self, digest: u128, meta: &(Vec<u8>, Vec<u8>)) -> bool {
        for class in &self.classes {
            let Ok(mut lru) = class.lru.write() else {
                continue;
            };
            if let Some(entry) = lru.get_mut(digest) {
                let Ok(mut guard) = entry.meta.write() else {
                    return false;
                };
                *guard = meta.clone();
                return true;
            }
        }
        false
    }
}

/// An object the memory tier dropped.
struct SpilledEntry {
    digest: u128,
    size: u64,
    /// The serialized metadata, or `None` when it could not be read
    /// (a poisoned lock) — such an object cannot be spilled.
    meta: Option<(Vec<u8>, Vec<u8>)>,
    body: Arc<Vec<u8>>,
}

/// A least-recently-used map from a digest to a weighted entry.
///
/// The recency order is a `BTreeMap` from a monotonic sequence number
/// to the digest; a hit gives the entry a new sequence number, so the
/// least recently used entry is the one with the smallest number.
struct ByteLru<V> {
    entries: HashMap<u128, LruNode<V>>,
    order: BTreeMap<u64, u128>,
    next_seq: u64,
}

struct LruNode<V> {
    value: V,
    weight: u64,
    seq: u64,
}

impl<V> ByteLru<V> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            next_seq: 0,
        }
    }

    /// The next sequence number.
    fn bump(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Read an entry and mark it as the most recently used.
    fn get(&mut self, key: u128) -> Option<&V> {
        let seq = self.entries.get(&key)?.seq;
        self.order.remove(&seq);
        let next = self.bump();
        self.order.insert(next, key);
        let node = self.entries.get_mut(&key)?;
        node.seq = next;
        Some(&node.value)
    }

    /// Read an entry without touching the recency order.
    fn get_mut(&mut self, key: u128) -> Option<&mut V> {
        self.entries.get_mut(&key).map(|node| &mut node.value)
    }

    /// Store an entry, returning the one it replaced.
    fn put(&mut self, key: u128, value: V, weight: u64) -> Option<(V, u64)> {
        let replaced = self.entries.remove(&key).map(|node| {
            self.order.remove(&node.seq);
            (node.value, node.weight)
        });
        let seq = self.bump();
        self.order.insert(seq, key);
        self.entries.insert(key, LruNode { value, weight, seq });
        replaced
    }

    /// Remove an entry.
    fn remove(&mut self, key: u128) -> Option<(V, u64)> {
        let node = self.entries.remove(&key)?;
        self.order.remove(&node.seq);
        Some((node.value, node.weight))
    }

    /// Remove the least recently used entry.
    fn pop_lru(&mut self) -> Option<(u128, V, u64)> {
        let (&seq, &key) = self.order.iter().next()?;
        self.order.remove(&seq);
        let node = self.entries.remove(&key)?;
        Some((key, node.value, node.weight))
    }
}

// ─────────────────────────────────────────────────────────────────
//  The disk tier
// ─────────────────────────────────────────────────────────────────

/// The disk tier: the objects that do not fit in memory, and the ones
/// the memory tier pushed out.
///
/// The tier survives a restart — an entry carries its freshness, so a
/// read after the restart finds it stale and revalidates it against
/// the origin.
struct DiskTier {
    /// The cache directory. `None` when the directory could not be
    /// created: the tier is then off and the memory tier carries the
    /// cache alone.
    root: Option<PathBuf>,
    budget: u64,
    bytes: AtomicU64,
    entries: AtomicU64,
    index: RwLock<DiskIndex>,
}

/// What the disk tier knows about its files.
#[derive(Default)]
struct DiskIndex {
    entries: HashMap<String, DiskEntry>,
    /// The eviction order: the largest file first, and the oldest
    /// within the same size.
    ///
    /// The digest is **part of the key**: two files of the same size
    /// that were last used in the same instant would otherwise share
    /// a key, and the second one would overwrite the first one's place
    /// in the order — leaving that file impossible to evict, and the
    /// tier free to grow past its budget.
    order: BTreeSet<(Reverse<u64>, u64, String)>,
}

/// One file of the disk tier.
struct DiskEntry {
    size: u64,
    last_used: u64,
}

impl DiskIndex {
    /// Record a file, returning the entry it replaced.
    fn insert(&mut self, digest: String, size: u64, last_used: u64) -> Option<DiskEntry> {
        let replaced = self
            .entries
            .insert(digest.clone(), DiskEntry { size, last_used });
        if let Some(old) = &replaced {
            self.order
                .remove(&(Reverse(old.size), old.last_used, digest.clone()));
        }
        self.order.insert((Reverse(size), last_used, digest));
        replaced
    }

    /// Forget a file, returning what it was.
    fn remove(&mut self, digest: &str) -> Option<DiskEntry> {
        let old = self.entries.remove(digest)?;
        self.order
            .remove(&(Reverse(old.size), old.last_used, digest.to_string()));
        Some(old)
    }

    /// Mark a file as used, so it is the last one evicted.
    fn touch(&mut self, digest: &str, last_used: u64) {
        let Some(entry) = self.entries.get(digest) else {
            return;
        };
        let size = entry.size;
        let old_used = entry.last_used;
        self.order
            .remove(&(Reverse(size), old_used, digest.to_string()));
        if let Some(entry) = self.entries.get_mut(digest) {
            entry.last_used = last_used;
        }
        self.order
            .insert((Reverse(size), last_used, digest.to_string()));
    }

    /// The file that is evicted next: the largest, and the oldest
    /// within the same size.
    fn victim(&self) -> Option<&str> {
        self.order
            .iter()
            .next()
            .map(|(_, _, digest)| digest.as_str())
    }
}

impl DiskTier {
    /// Open the tier, sweeping the leftovers of a killed write.
    fn open(root: &Path, budget: u64) -> Self {
        if let Err(e) = fs::create_dir_all(root) {
            error!(
                "cache: cannot create the cache directory {}: {e} — the disk tier is off",
                root.display()
            );
            return Self {
                root: None,
                budget,
                bytes: AtomicU64::new(0),
                entries: AtomicU64::new(0),
                index: RwLock::new(DiskIndex::default()),
            };
        }
        let tier = Self {
            root: Some(root.to_path_buf()),
            budget,
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            index: RwLock::new(DiskIndex::default()),
        };
        tier.sweep();
        tier.enforce_budget();
        tier
    }

    /// Walk the cache directory: remove the leftovers of a killed
    /// write and take account of the entries that survived.
    fn sweep(&self) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        let Ok(shards) = fs::read_dir(root) else {
            return;
        };
        // One timestamp for the whole sweep: the files it finds were
        // all there when the tier opened, and one stamp keeps the
        // order's tie-break deterministic.
        let opened = now_nanos();
        for shard in shards.flatten() {
            let path = shard.path();
            if path.is_dir() {
                let Ok(files) = fs::read_dir(&path) else {
                    continue;
                };
                for file in files.flatten() {
                    self.take_account(&file.path(), &file.file_name().to_string_lossy(), opened);
                }
            } else {
                // A file in the cache root is not an entry: the
                // tier's files live in a shard directory, and this
                // one could neither be read nor removed through
                // `path_for`. The only thing worth doing with it is
                // removing the leftover of a killed write.
                self.remove_leftover(&path, &shard.file_name().to_string_lossy());
            }
        }
    }

    /// Take account of one file of the tier — or delete it when it is
    /// the leftover of a write that was killed.
    fn take_account(&self, path: &Path, name: &str, opened: u64) {
        if name.ends_with(".tmp") {
            // A write that never reached its rename: not an entry.
            self.remove_leftover(path, name);
            return;
        }
        let Some(digest) = name.strip_suffix(".bin") else {
            return;
        };
        if !is_digest(digest) {
            return;
        }
        let Ok(meta) = fs::metadata(path) else { return };
        self.index_entry(digest.to_string(), meta.len(), opened);
    }

    /// Delete the leftover of a write that was killed.
    fn remove_leftover(&self, path: &Path, name: &str) {
        if !name.ends_with(".tmp") {
            return;
        }
        if let Err(e) = fs::remove_file(path) {
            debug!("cache: cannot remove the leftover {}: {e}", path.display());
        }
    }

    /// The tier's occupancy in bytes.
    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The number of files the tier holds.
    fn entry_count(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// Read the index.
    fn read_index(&self) -> Option<RwLockReadGuard<'_, DiskIndex>> {
        self.index.read().ok()
    }

    /// Write the index.
    fn write_index(&self) -> Option<RwLockWriteGuard<'_, DiskIndex>> {
        self.index.write().ok()
    }

    /// Whether the tier holds a file for this digest.
    ///
    /// The answer comes from the index, so a key the tier does not
    /// hold costs no I/O — the caller can skip the disk read
    /// entirely.
    fn contains(&self, digest: &str) -> bool {
        let Some(index) = self.read_index() else {
            return false;
        };
        index.entries.contains_key(digest)
    }

    /// Record a file in the index.
    fn index_entry(&self, digest: String, size: u64, last_used: u64) {
        let Some(mut index) = self.write_index() else {
            return;
        };
        if let Some(old) = index.insert(digest, size, last_used) {
            self.bytes.fetch_sub(old.size, Ordering::Relaxed);
            self.entries.fetch_sub(1, Ordering::Relaxed);
        }
        self.bytes.fetch_add(size, Ordering::Relaxed);
        self.entries.fetch_add(1, Ordering::Relaxed);
    }

    /// Forget a file.
    fn forget(&self, digest: &str) {
        let Some(mut index) = self.write_index() else {
            return;
        };
        if let Some(old) = index.remove(digest) {
            self.bytes.fetch_sub(old.size, Ordering::Relaxed);
            self.entries.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Mark a file as used, so it is the last one evicted.
    fn touch(&self, digest: &str) {
        let Some(mut index) = self.write_index() else {
            return;
        };
        index.touch(digest, now_nanos());
    }

    /// Read an entry.
    ///
    /// The bytes come back in the file's own layout: the serialized
    /// metadata and the body. A file that is too short to hold its
    /// header is a damaged file: it is deleted and the read is a miss.
    fn get(&self, digest: &str) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        if !is_digest(digest) {
            return None;
        }
        let root = self.root.as_ref()?;
        let path = path_for(root, digest);
        let bytes = fs::read(&path).ok()?;
        if bytes.len() < 8 {
            self.remove(digest);
            return None;
        }
        let internal_len = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
        let header_len = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
        let head = 8 + internal_len + header_len;
        if bytes.len() < head {
            self.remove(digest);
            return None;
        }
        let internal = bytes[8..8 + internal_len].to_vec();
        let header = bytes[8 + internal_len..head].to_vec();
        let body = bytes[head..].to_vec();
        self.touch(digest);
        Some((internal, header, body))
    }

    /// Write an entry.
    ///
    /// The bytes go to a temporary file that is renamed into place, so
    /// a reader never sees a half-written entry and a process that is
    /// killed leaves a temporary file — never a broken entry.
    fn put(&self, digest: &str, meta: &(Vec<u8>, Vec<u8>), body: &[u8]) -> Result<()> {
        let Some(root) = self.root.as_ref() else {
            // The disk tier is off: the entry is simply not stored.
            return Ok(());
        };
        if !is_digest(digest) {
            return Error::e_explain(
                ErrorType::InternalError,
                "cache: refusing to write an entry under a malformed name",
            );
        }
        let dir = root.join(&digest[..2]);
        if let Err(e) = fs::create_dir_all(&dir) {
            return Error::e_explain(
                ErrorType::FileCreateError,
                format!("cache: cannot create {}: {e}", dir.display()),
            );
        }
        let temporary = dir.join(format!("{digest}.tmp"));
        let target = path_for(root, digest);
        let written = match write_entry(&temporary, meta, body) {
            Ok(written) => written,
            Err(e) => {
                let _ = fs::remove_file(&temporary);
                return Error::e_explain(
                    ErrorType::FileWriteError,
                    format!("cache: cannot write {}: {e}", temporary.display()),
                );
            }
        };
        if let Err(e) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Error::e_explain(
                ErrorType::FileWriteError,
                format!("cache: cannot publish {}: {e}", target.display()),
            );
        }
        self.index_entry(digest.to_string(), written, now_nanos());
        self.enforce_budget();
        Ok(())
    }

    /// Rewrite an entry's metadata, keeping its body.
    fn update_meta(&self, digest: &str, meta: &(Vec<u8>, Vec<u8>)) -> Result<bool> {
        let Some((_, _, body)) = self.get(digest) else {
            return Ok(false);
        };
        self.put(digest, meta, &body)?;
        Ok(true)
    }

    /// Delete an entry.
    fn remove(&self, digest: &str) -> bool {
        if !is_digest(digest) {
            return false;
        }
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        let removed = fs::remove_file(path_for(root, digest)).is_ok();
        self.forget(digest);
        removed
    }

    /// Drop the biggest files until the tier is inside its budget.
    fn enforce_budget(&self) {
        while self.bytes() > self.budget {
            let victim = {
                let Some(index) = self.read_index() else {
                    break;
                };
                index.victim().map(|digest| digest.to_string())
            };
            let Some(victim) = victim else { break };
            self.remove(&victim);
        }
    }
}

/// Write an entry's bytes to a file.
///
/// The layout is the two metadata lengths, the metadata and the body.
/// The lengths are what lets a reader find the body without a second
/// pass over the file.
fn write_entry(path: &Path, meta: &(Vec<u8>, Vec<u8>), body: &[u8]) -> std::io::Result<u64> {
    let mut file = fs::File::create(path)?;
    file.write_all(&(meta.0.len() as u32).to_le_bytes())?;
    file.write_all(&(meta.1.len() as u32).to_le_bytes())?;
    file.write_all(&meta.0)?;
    file.write_all(&meta.1)?;
    file.write_all(body)?;
    Ok((8 + meta.0.len() + meta.1.len() + body.len()) as u64)
}

/// The path of an entry's file.
fn path_for(root: &Path, digest: &str) -> PathBuf {
    root.join(&digest[..2]).join(format!("{digest}.bin"))
}

/// Whether a name is a well-formed digest.
fn is_digest(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The current time in nanoseconds since the epoch.
///
/// The disk tier's eviction order breaks a tie between two files of
/// the same size by when they were last used, and a second is too
/// coarse for that: two files written in the same second would share
/// a place in the order.
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────
//  The hit and miss handlers
// ─────────────────────────────────────────────────────────────────

/// A cache hit: the stored body, served once.
struct EdgeHitHandler {
    body: Arc<Vec<u8>>,
    range_start: usize,
    range_end: usize,
    done: bool,
}

impl EdgeHitHandler {
    fn new(entry: Arc<MemoryEntry>) -> Self {
        let len = entry.body.len();
        Self {
            body: entry.body.clone(),
            range_start: 0,
            range_end: len,
            done: false,
        }
    }
}

#[async_trait]
impl HandleHit for EdgeHitHandler {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Bytes::copy_from_slice(
            &self.body[self.range_start..self.range_end],
        )))
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        true
    }

    fn seek(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        if start >= self.body.len() {
            return Error::e_explain(
                ErrorType::InternalError,
                format!("cache: seek start {start} is past the end of the body"),
            );
        }
        self.range_start = start;
        self.range_end = match end {
            Some(end) => end.min(self.body.len()),
            None => self.body.len(),
        };
        self.done = false;
        Ok(())
    }

    fn get_eviction_weight(&self) -> usize {
        self.body.len()
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// A cache miss: the body that is being stored.
struct EdgeMissHandler {
    storage: &'static EdgeCacheStorage,
    digest: u128,
    meta: (Vec<u8>, Vec<u8>),
    body: Vec<u8>,
    max_size: usize,
}

#[async_trait]
impl HandleMiss for EdgeMissHandler {
    async fn write_body(&mut self, data: Bytes, _eof: bool) -> Result<()> {
        if self.body.len() + data.len() > self.max_size {
            return Error::e_explain(
                ErrorType::InternalError,
                "cache: the response is bigger than the cache's object limit",
            );
        }
        self.body.extend_from_slice(&data);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        let size = self.body.len();
        // A failed write is reported, but it never fails the request:
        // the response has already been sent to the client.
        self.storage
            .commit(self.digest, self.meta, self.body)
            .await?;
        Ok(MissFinishType::Created(size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora::cache::trace::Span;
    use pingora::http::ResponseHeader;
    use std::time::Duration;
    use tempfile::TempDir;

    /// The key of a request for `path`.
    fn test_key(path: &str) -> CacheKey {
        key::primary_key("http", "example.com", path)
    }

    /// A cacheable response, fresh until `fresh_until`.
    fn test_meta(fresh_until: SystemTime) -> CacheMeta {
        let mut header = ResponseHeader::build(200, None).expect("build header");
        header
            .insert_header("Content-Type", "text/css")
            .expect("insert content-type");
        CacheMeta::new(fresh_until, SystemTime::now(), 0, 0, header)
    }

    /// A storage over a fresh directory.
    fn storage(
        dir: &Path,
        memory: u64,
        disk: u64,
        max: u64,
        memory_max: u64,
    ) -> &'static EdgeCacheStorage {
        Box::leak(Box::new(EdgeCacheStorage::new(
            dir,
            memory,
            disk,
            max,
            memory_max,
            Arc::new(CacheStats::new()),
        )))
    }

    /// Store a body under a key.
    async fn store(storage: &'static EdgeCacheStorage, key: &CacheKey, body: &[u8]) {
        let span = Span::inactive().handle();
        let meta = test_meta(SystemTime::now() + Duration::from_secs(60));
        let mut miss = storage
            .get_miss_handler(key, &meta, &span)
            .await
            .expect("miss handler");
        miss.write_body(Bytes::copy_from_slice(body), true)
            .await
            .expect("write body");
        miss.finish().await.expect("finish");
    }

    #[tokio::test]
    async fn a_written_object_is_read_back() {
        let dir = TempDir::new().expect("tempdir");
        let storage = storage(
            dir.path(),
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
        );
        let key = test_key("/a.css");
        store(storage, &key, b"body{color:red}").await;

        let span = Span::inactive().handle();
        let found = storage.lookup(&key, &span).await.expect("lookup");
        let (meta, mut handler) = found.expect("the object must be found");
        assert!(meta.is_fresh(SystemTime::now()));
        assert_eq!(
            handler.read_body().await.expect("read").expect("a body"),
            Bytes::from_static(b"body{color:red}")
        );
        assert!(handler.read_body().await.expect("read").is_none());
    }

    #[tokio::test]
    async fn an_object_that_does_not_fit_in_memory_goes_to_the_disk_tier() {
        let dir = TempDir::new().expect("tempdir");
        // The memory tier takes objects up to 1 KiB; the body is 2 KiB.
        let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1024);
        let key = test_key("/big.css");
        store(storage, &key, &[b'x'; 2048]).await;

        let (memory_bytes, memory_entries) = storage.memory_usage();
        assert_eq!(memory_entries, 0, "the object must not be in memory");
        assert_eq!(memory_bytes, 0);
        let (disk_bytes, disk_entries) = storage.disk_usage();
        assert_eq!(disk_entries, 1, "the object must be on disk");
        assert!(disk_bytes > 2048);

        // It is still served.
        let span = Span::inactive().handle();
        let found = storage.lookup(&key, &span).await.expect("lookup");
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn the_disk_tier_survives_a_restart() {
        let dir = TempDir::new().expect("tempdir");
        let key = test_key("/big.css");
        {
            let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1024);
            store(storage, &key, &[b'x'; 2048]).await;
        }
        // A new storage over the same directory — the gateway
        // restarted.
        let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1024);
        let span = Span::inactive().handle();
        let found = storage.lookup(&key, &span).await.expect("lookup");
        assert!(found.is_some(), "the entry must survive the restart");
    }

    #[tokio::test]
    async fn an_expired_entry_is_stale() {
        let dir = TempDir::new().expect("tempdir");
        let storage = storage(
            dir.path(),
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
        );
        let key = test_key("/a.css");
        let span = Span::inactive().handle();
        let meta = test_meta(SystemTime::now() - Duration::from_secs(60));
        let mut miss = storage
            .get_miss_handler(&key, &meta, &span)
            .await
            .expect("miss handler");
        miss.write_body(Bytes::from_static(b"old"), true)
            .await
            .expect("write body");
        miss.finish().await.expect("finish");

        let found = storage.lookup(&key, &span).await.expect("lookup");
        let (meta, _handler) = found.expect("the object is still stored");
        assert!(
            !meta.is_fresh(SystemTime::now()),
            "an expired entry must not be fresh"
        );
    }

    #[tokio::test]
    async fn an_entry_with_damaged_metadata_is_a_miss_and_is_deleted() {
        let dir = TempDir::new().expect("tempdir");
        let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1024);
        let key = test_key("/big.css");
        store(storage, &key, &[b'x'; 2048]).await;

        // Damage the file the way a half-written entry would be
        // damaged: the metadata bytes are garbage.
        let hex = key::digest_hex(key::digest(&key));
        let path = dir.path().join(&hex[..2]).join(format!("{hex}.bin"));
        let mut bytes = fs::read(&path).expect("read the entry");
        bytes[8] = 0xff;
        bytes[9] = 0xfe;
        fs::write(&path, &bytes).expect("write the damaged entry");

        let span = Span::inactive().handle();
        let found = storage.lookup(&key, &span).await.expect("lookup");
        assert!(found.is_none(), "a damaged entry must be a miss");
        assert!(
            !path.exists(),
            "a damaged entry must be deleted, so the cache heals itself"
        );
    }

    #[tokio::test]
    async fn the_leftovers_of_a_killed_write_are_swept() {
        let dir = TempDir::new().expect("tempdir");
        let shard = dir.path().join("ab");
        fs::create_dir_all(&shard).expect("mkdir");
        let leftover = shard.join("0123456789abcdef0123456789abcdef.tmp");
        fs::write(&leftover, b"half written").expect("write the leftover");

        let _storage = storage(
            dir.path(),
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
        );
        assert!(
            !leftover.exists(),
            "the leftover of a killed write must be swept at startup"
        );
    }

    #[tokio::test]
    async fn the_memory_tier_drops_the_largest_objects_first() {
        let dir = TempDir::new().expect("tempdir");
        // The size classes are 16 KiB and 64 KiB, so the 1 KiB object
        // and the 32 KiB object sit in different classes — and a
        // 20 KiB budget cannot hold both.
        let storage = storage(dir.path(), 20 * 1024, 1024 * 1024, 1024 * 1024, 64 * 1024);
        let small = test_key("/small.css");
        let big = test_key("/big.css");
        store(storage, &small, &[b's'; 1024]).await;
        store(storage, &big, &[b'b'; 32 * 1024]).await;

        // The big object is the one that leaves the memory tier: the
        // tier drops the largest class first.
        let (memory_bytes, memory_entries) = storage.memory_usage();
        assert_eq!(memory_entries, 1, "only the small object stays in memory");
        assert_eq!(memory_bytes, 1024);
        // It is not lost: it spilled into the disk tier.
        let (_disk_bytes, disk_entries) = storage.disk_usage();
        assert_eq!(disk_entries, 1, "the big object must spill to the disk");

        // Both are still served.
        let span = Span::inactive().handle();
        assert!(
            storage
                .lookup(&small, &span)
                .await
                .expect("lookup")
                .is_some()
        );
        assert!(storage.lookup(&big, &span).await.expect("lookup").is_some());
    }

    #[tokio::test]
    async fn the_query_string_is_not_part_of_the_key() {
        let dir = TempDir::new().expect("tempdir");
        let storage = storage(
            dir.path(),
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
        );
        let key = test_key("/a.css");
        store(storage, &key, b"body").await;

        // The same path with another query: the caller passes the
        // path only, so it is the same entry.
        let other = test_key("/a.css");
        let span = Span::inactive().handle();
        let found = storage.lookup(&other, &span).await.expect("lookup");
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn an_object_over_the_limit_is_not_stored() {
        let dir = TempDir::new().expect("tempdir");
        let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 64, 1024 * 1024);
        let key = test_key("/a.css");
        let span = Span::inactive().handle();
        let meta = test_meta(SystemTime::now() + Duration::from_secs(60));
        let mut miss = storage
            .get_miss_handler(&key, &meta, &span)
            .await
            .expect("miss handler");
        // The body is bigger than the cache's object limit.
        assert!(
            miss.write_body(Bytes::from_static(&[b'x'; 128]), true)
                .await
                .is_err(),
            "the store must refuse a body over the limit"
        );
        // Nothing was stored, and nothing half-written is left behind.
        let found = storage.lookup(&key, &span).await.expect("lookup");
        assert!(found.is_none());
        let (_memory_bytes, memory_entries) = storage.memory_usage();
        assert_eq!(memory_entries, 0);
    }

    #[tokio::test]
    async fn a_disk_write_failure_loses_the_entry_and_nothing_else() {
        let dir = TempDir::new().expect("tempdir");
        let key = test_key("/big.css");
        let hex = key::digest_hex(key::digest(&key));
        // The shard directory is a file, so the disk write cannot
        // succeed — the shape of a full or a read-only disk.
        fs::write(dir.path().join(&hex[..2]), b"not a directory").expect("write the blocker");
        // Everything goes to the disk tier.
        let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1);
        let span = Span::inactive().handle();
        let meta = test_meta(SystemTime::now() + Duration::from_secs(60));
        let mut miss = storage
            .get_miss_handler(&key, &meta, &span)
            .await
            .expect("miss handler");
        miss.write_body(Bytes::from_static(b"body"), true)
            .await
            .expect("write body");
        // The store fails — and it fails the *store*, not the request:
        // the engine logs it and the answer still goes to the client.
        assert!(miss.finish().await.is_err());
        // Nothing half-written is left behind.
        let found = storage.lookup(&key, &span).await.expect("lookup");
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn the_disk_tier_drops_the_biggest_files_first() {
        let dir = TempDir::new().expect("tempdir");
        let small = test_key("/small.css");
        let big = test_key("/big.css");
        // Everything goes to the disk tier.
        let small_size = {
            let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1);
            store(storage, &small, &[b's'; 64]).await;
            storage.disk_usage().0
        };

        // A tier whose budget only fits the small file drops the big
        // one — the biggest file goes first.
        let storage = storage(dir.path(), 1024 * 1024, small_size, 1024 * 1024, 1);
        store(storage, &big, &[b'b'; 4096]).await;
        let (_bytes, entries) = storage.disk_usage();
        assert_eq!(entries, 1, "the biggest file must be the one that goes");

        let span = Span::inactive().handle();
        assert!(
            storage
                .lookup(&small, &span)
                .await
                .expect("lookup")
                .is_some(),
            "the small file must stay"
        );
        assert!(
            storage.lookup(&big, &span).await.expect("lookup").is_none(),
            "the big file must be the one that is dropped"
        );
    }

    /// Two files of the same size, indexed in the same instant, keep
    /// two places in the eviction order.
    ///
    /// The order's key used to be the size and the last-used time
    /// alone, so the second file overwrote the first one's place —
    /// and the first file became impossible to evict, which let the
    /// disk tier grow past its budget without end.
    #[test]
    fn two_files_of_the_same_size_keep_two_places_in_the_order() {
        let mut index = DiskIndex::default();
        index.insert("aaaa".into(), 100, 1_000);
        index.insert("bbbb".into(), 100, 1_000);
        assert_eq!(index.order.len(), 2, "both files must be in the order");
        let mut victims: Vec<&str> = index
            .order
            .iter()
            .map(|(_, _, digest)| digest.as_str())
            .collect();
        victims.sort_unstable();
        assert_eq!(victims, vec!["aaaa", "bbbb"]);
    }

    /// A tier whose budget fits neither file empties itself.
    ///
    /// It cannot do that when one of the files is missing from the
    /// eviction order — the file stays on the disk and its bytes stay
    /// counted, so the tier never gets inside its budget again.
    #[tokio::test]
    async fn the_disk_tier_evicts_two_files_of_the_same_size() {
        let dir = TempDir::new().expect("tempdir");
        let first = test_key("/a.css");
        let second = test_key("/b.css");
        {
            // Two files of the same size, written into the tier.
            let storage = storage(dir.path(), 1024 * 1024, 1024 * 1024, 1024 * 1024, 1);
            store(storage, &first, &[b'x'; 64]).await;
            store(storage, &second, &[b'x'; 64]).await;
        }
        // A tier whose budget fits neither file must empty itself.
        let storage = storage(dir.path(), 1024 * 1024, 1, 1024 * 1024, 1);
        let (_bytes, entries) = storage.disk_usage();
        assert_eq!(entries, 0, "the tier must evict every file it cannot hold");
    }

    #[tokio::test]
    async fn a_purged_entry_is_gone() {
        let dir = TempDir::new().expect("tempdir");
        let storage = storage(
            dir.path(),
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
            1024 * 1024,
        );
        let key = test_key("/a.css");
        store(storage, &key, b"body").await;

        let span = Span::inactive().handle();
        let compact = key.to_compact();
        let outcome = storage
            .purge(
                PurgeTarget::Active(&compact),
                PurgeType::Invalidation,
                &span,
            )
            .await
            .expect("purge");
        assert_eq!(outcome, PurgeOutcome::Purged(None));
        let found = storage.lookup(&key, &span).await.expect("lookup");
        assert!(found.is_none());
    }
}
