use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::crypto::Crypto;
use crate::now_secs;

// ---------------------------------------------------------------------------
// CacheTier
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheTier {
    /// Playlists, liked tracks, favorites.  TTL: 15 min.
    UserContent,
    /// Artist bios, top charts, home page.  TTL: 4 hours.
    Dynamic,
    /// Album tracklists, credits.           TTL: 7 days.
    StaticMeta,
    /// Album art, avatars.                  TTL: 30 days.
    Image,
}

impl CacheTier {
    pub fn ttl(&self) -> Duration {
        match self {
            CacheTier::UserContent => Duration::from_secs(15 * 60),
            CacheTier::Dynamic => Duration::from_secs(4 * 60 * 60),
            CacheTier::StaticMeta => Duration::from_secs(7 * 24 * 60 * 60),
            CacheTier::Image => Duration::from_secs(30 * 24 * 60 * 60),
        }
    }

    /// How long past TTL we still serve stale data while refreshing.
    pub fn swr_grace(&self) -> Duration {
        match self {
            CacheTier::UserContent => Duration::from_secs(60 * 60),
            CacheTier::Dynamic => Duration::from_secs(24 * 60 * 60),
            CacheTier::StaticMeta => Duration::from_secs(30 * 24 * 60 * 60),
            CacheTier::Image => Duration::from_secs(90 * 24 * 60 * 60),
        }
    }

    fn subdir(&self) -> &'static str {
        match self {
            CacheTier::UserContent => "user",
            CacheTier::Dynamic => "dynamic",
            CacheTier::StaticMeta => "static",
            CacheTier::Image => "images",
        }
    }
}

const ALL_TIERS: [CacheTier; 4] = [
    CacheTier::UserContent,
    CacheTier::Dynamic,
    CacheTier::StaticMeta,
    CacheTier::Image,
];

// ---------------------------------------------------------------------------
// Cache statistics (public API)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub total_entries: usize,
    pub total_disk_mb: f64,
    pub max_disk_mb: f64,
    pub usage_percent: f64,
    pub by_tier: HashMap<String, TierStats>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TierStats {
    pub count: usize,
    pub size_mb: f64,
}

// ---------------------------------------------------------------------------
// CacheResult
// ---------------------------------------------------------------------------

pub enum CacheResult {
    /// Data is within TTL — no refresh needed.
    Fresh(Vec<u8>),
    /// Data is past TTL but within SWR grace — caller should return this AND
    /// trigger a background refresh.
    Stale(Vec<u8>),
    /// Not found or expired beyond SWR grace.
    Miss,
}

/// Capture before looking up or fetching data, not when the network completes.
#[derive(Clone, Copy)]
pub struct FetchTicket {
    generation: u64,
}

/// Only the job that acquired this lease can release its refresh marker.
pub struct RefreshTicket {
    hash: String,
    id: u64,
}

// ---------------------------------------------------------------------------
// On-disk metadata per entry
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
struct EntryMeta {
    schema_version: u8,
    tags: Vec<String>,
    tier: CacheTier,
    created_at: u64,
    size: u64,
}

// ---------------------------------------------------------------------------
// In-memory index entry
// ---------------------------------------------------------------------------

struct IndexEntry {
    version: u64,
    tier: CacheTier,
    tags: Vec<String>,
    created_at: u64,
    size: u64,
    last_access: u64,
    last_refresh_attempt: Option<u64>,
}

// ---------------------------------------------------------------------------
// Inner state (behind RwLock)
// ---------------------------------------------------------------------------

struct DiskCacheInner {
    /// hash → index entry
    index: HashMap<String, IndexEntry>,
    /// tag → set of hashes
    tag_index: HashMap<String, HashSet<String>>,
    /// Monotonic counter for LRU ordering.
    access_counter: u64,
    /// Total bytes of all `.dat` files on disk.
    total_disk_usage: u64,
    /// Keys currently being refreshed (prevents duplicate SWR spawns).
    in_flight: HashMap<String, u64>,
    generation: u64,
    cleared_at: u64,
    invalidated_keys: HashMap<String, u64>,
    invalidated_tags: HashMap<String, u64>,
    next_refresh_id: u64,
}

impl DiskCacheInner {
    fn new() -> Self {
        Self {
            index: HashMap::new(),
            tag_index: HashMap::new(),
            access_counter: 0,
            total_disk_usage: 0,
            in_flight: HashMap::new(),
            generation: 0,
            cleared_at: 0,
            invalidated_keys: HashMap::new(),
            invalidated_tags: HashMap::new(),
            next_refresh_id: 0,
        }
    }

    fn is_current(&self, ticket: &FetchTicket, hash: &str, tags: &[&str]) -> bool {
        ticket.generation >= self.cleared_at
            && self
                .invalidated_keys
                .get(hash)
                .is_none_or(|epoch| ticket.generation >= *epoch)
            && tags.iter().all(|tag| {
                self.invalidated_tags
                    .get(*tag)
                    .is_none_or(|epoch| ticket.generation >= *epoch)
            })
    }

    fn add_to_tag_index(&mut self, hash: &str, tags: &[String]) {
        for tag in tags {
            self.tag_index
                .entry(tag.clone())
                .or_default()
                .insert(hash.to_string());
        }
    }

    fn remove_from_tag_index(&mut self, hash: &str, tags: &[String]) {
        // Removing metadata must not scan the much larger image collection.
        // Each entry already records exactly which tag sets contain it.
        for tag in tags {
            if let Some(hashes) = self.tag_index.get_mut(tag) {
                hashes.remove(hash);
                if hashes.is_empty() {
                    self.tag_index.remove(tag);
                }
            }
        }
    }

    fn remove_entry(&mut self, hash: &str) -> Option<IndexEntry> {
        if let Some(entry) = self.index.remove(hash) {
            self.total_disk_usage = self.total_disk_usage.saturating_sub(entry.size);
            self.remove_from_tag_index(hash, &entry.tags);
            Some(entry)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// DiskCache
// ---------------------------------------------------------------------------

const MAX_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
const EVICT_TARGET: u64 = MAX_DISK_BYTES * 9 / 10; // 1.8 GB
const CURRENT_SCHEMA_VERSION: u8 = 5;

pub struct DiskCache {
    base_dir: PathBuf,
    inner: RwLock<DiskCacheInner>,
    crypto: Arc<Crypto>,
}

impl DiskCache {
    /// Create cache and rebuild the in-memory index by scanning disk.
    /// The actual storage lives under `cache_dir/v{CURRENT_SCHEMA_VERSION}/`.
    pub fn new(cache_dir: &Path, crypto: Arc<Crypto>) -> Self {
        let base_dir = cache_dir.join(format!("v{CURRENT_SCHEMA_VERSION}"));

        // Delete old version folders.
        if let Ok(entries) = fs::read_dir(cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if name.starts_with('v') && name != format!("v{CURRENT_SCHEMA_VERSION}") {
                    log::info!("[DiskCache] removing old cache folder: {name}");
                    fs::remove_dir_all(&path).ok();
                }
            }
        }

        // Ensure tier subdirs exist.
        for tier in &ALL_TIERS {
            fs::create_dir_all(base_dir.join(tier.subdir())).ok();
        }

        let mut inner = DiskCacheInner::new();
        let now = now_secs();

        // Scan each tier subdir for .meta files and rebuild the index.
        for tier in &ALL_TIERS {
            let dir = base_dir.join(tier.subdir());
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries.flatten() {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                // Only process .meta files
                if !name.ends_with(".meta") {
                    continue;
                }
                let hash = name.trim_end_matches(".meta").to_string();
                let dat_path = dir.join(format!("{}.dat", hash));

                // Read and parse meta
                let meta: EntryMeta = match fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                {
                    Some(m) => m,
                    None => {
                        // Corrupt meta — delete both files.
                        fs::remove_file(&path).ok();
                        fs::remove_file(&dat_path).ok();
                        continue;
                    }
                };

                // Check schema version — delete if mismatch.
                if meta.schema_version != CURRENT_SCHEMA_VERSION {
                    log::debug!(
                        "[DiskCache] schema mismatch: {} (expected {}), deleting {}",
                        meta.schema_version,
                        CURRENT_SCHEMA_VERSION,
                        hash
                    );
                    fs::remove_file(&path).ok();
                    fs::remove_file(&dat_path).ok();
                    continue;
                }

                // Check if entry is beyond TTL + SWR grace (fully expired).
                let max_age = meta.tier.ttl().as_secs() + meta.tier.swr_grace().as_secs();
                let age = now.saturating_sub(meta.created_at);
                if age > max_age {
                    fs::remove_file(&path).ok();
                    fs::remove_file(&dat_path).ok();
                    continue;
                }

                // Verify dat file exists
                if !dat_path.exists() {
                    fs::remove_file(&path).ok();
                    continue;
                }

                inner.access_counter += 1;
                inner.total_disk_usage += meta.size;
                inner.add_to_tag_index(&hash, &meta.tags);
                inner.index.insert(
                    hash,
                    IndexEntry {
                        version: 0,
                        tier: meta.tier,
                        tags: meta.tags,
                        created_at: meta.created_at,
                        size: meta.size,
                        last_access: inner.access_counter,
                        last_refresh_attempt: None,
                    },
                );
            }
        }

        log::info!(
            "[DiskCache] rebuilt index: {} entries, {:.1} MB on disk",
            inner.index.len(),
            inner.total_disk_usage as f64 / (1024.0 * 1024.0)
        );

        Self {
            base_dir,
            inner: RwLock::new(inner),
            crypto,
        }
    }

    /// Look up a cache entry. Returns Fresh / Stale / Miss.
    pub async fn get(&self, key: &str, tier: CacheTier) -> CacheResult {
        let hash = hash_key(key);

        // Read-lock: check index.
        let (created_at, version) = {
            let inner = self.inner.read().await;
            match inner.index.get(&hash) {
                Some(entry) if entry.tier == tier => (entry.created_at, Some(entry.version)),
                _ => (0, None),
            }
        };

        let Some(version) = version else {
            log::debug!("[DiskCache] MISS: {} (tier={:?})", key, tier);
            return CacheResult::Miss;
        };

        let age = now_secs().saturating_sub(created_at);
        let ttl = tier.ttl().as_secs();
        let grace = tier.swr_grace().as_secs();
        if age >= ttl + grace {
            log::debug!(
                "[DiskCache] MISS (expired): {} (tier={:?}, age={}s)",
                key,
                tier,
                age
            );
            // The index already proves this data unusable. Skip reading and
            // decrypting it only to throw the resulting allocation away.
            self.remove_version(&hash, tier, version).await;
            return CacheResult::Miss;
        }

        // Read data from disk (outside lock) and decrypt.
        let dat_path = self
            .base_dir
            .join(tier.subdir())
            .join(format!("{}.dat", hash));
        let data = match fs::read(&dat_path) {
            Ok(raw) => match self.crypto.decrypt(&raw) {
                Ok(plain) => plain,
                Err(e) => {
                    // Decryption failed (key changed?) — treat as miss, remove corrupt entry.
                    log::warn!("[DiskCache] decrypt failed for {}: {e}", &hash[..12]);
                    self.remove_version(&hash, tier, version).await;
                    return CacheResult::Miss;
                }
            },
            Err(_) => {
                // File gone but index has it — clean up.
                self.remove_version(&hash, tier, version).await;
                return CacheResult::Miss;
            }
        };

        // A concurrent publication/invalidation may have replaced the file we read.
        // Never return it or touch the new entry on behalf of this old lookup.
        // Update LRU access counter.
        {
            let mut inner = self.inner.write().await;
            if inner
                .index
                .get(&hash)
                .is_none_or(|entry| entry.version != version)
            {
                return CacheResult::Miss;
            }
            inner.access_counter += 1;
            let counter = inner.access_counter;
            if let Some(entry) = inner.index.get_mut(&hash) {
                entry.last_access = counter;
            }
        }

        if age < ttl {
            log::debug!(
                "[DiskCache] HIT (fresh): {} (tier={:?}, age={}s)",
                key,
                tier,
                age
            );
            CacheResult::Fresh(data)
        } else {
            log::debug!(
                "[DiskCache] HIT (stale): {} (tier={:?}, age={}s, refresh needed)",
                key,
                tier,
                age
            );
            CacheResult::Stale(data)
        }
    }

    async fn remove_version(&self, hash: &str, tier: CacheTier, version: u64) {
        let mut inner = self.inner.write().await;
        if inner
            .index
            .get(hash)
            .is_some_and(|entry| entry.version == version)
        {
            self.remove_files(hash, tier);
            inner.remove_entry(hash);
        }
    }

    /// Capture the cache's invalidation epoch before any request is started.
    pub async fn begin_fetch(&self) -> FetchTicket {
        FetchTicket {
            generation: self.inner.read().await.generation,
        }
    }

    /// Publish only if no clear, key invalidation or matching tag invalidation
    /// happened since the request began. Publication and invalidation use the
    /// same write lock, including their filesystem changes.
    pub async fn put_if_current(
        &self,
        ticket: &FetchTicket,
        key: &str,
        data: &[u8],
        tier: CacheTier,
        tags: &[&str],
    ) -> Result<bool, std::io::Error> {
        let hash = hash_key(key);
        let encrypted = self
            .crypto
            .encrypt(data)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tag_strings: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
        let created_at = now_secs();
        let size = data.len() as u64;
        let meta = EntryMeta {
            schema_version: CURRENT_SCHEMA_VERSION,
            tags: tag_strings.clone(),
            tier,
            created_at,
            size,
        };
        let meta_bytes = serde_json::to_vec(&meta).map_err(std::io::Error::other)?;
        {
            let mut inner = self.inner.write().await;
            if !inner.is_current(ticket, &hash, tags) {
                return Ok(false);
            }
            let dir = self.base_dir.join(tier.subdir());
            fs::write(dir.join(format!("{hash}.dat")), encrypted)?;
            fs::write(dir.join(format!("{hash}.meta")), meta_bytes)?;
            inner.remove_entry(&hash);
            inner.generation += 1;
            let version = inner.generation;
            inner.access_counter += 1;
            let last_access = inner.access_counter;
            inner.total_disk_usage += size;
            inner.add_to_tag_index(&hash, &tag_strings);
            inner.index.insert(
                hash,
                IndexEntry {
                    version,
                    tier,
                    tags: tag_strings,
                    created_at,
                    size,
                    last_access,
                    last_refresh_attempt: None,
                },
            );
        }
        self.maybe_evict().await;
        Ok(true)
    }

    // Fixtures seed data immediately; production callers must obtain a ticket
    // before fetching so there is no accidentally unfenced write API.
    #[cfg(test)]
    pub async fn put(
        &self,
        key: &str,
        data: &[u8],
        tier: CacheTier,
        tags: &[&str],
    ) -> Result<(), std::io::Error> {
        let ticket = self.begin_fetch().await;
        self.put_if_current(&ticket, key, data, tier, tags)
            .await
            .map(|_| ())
    }

    /// Advance the tag epoch even if nothing is cached yet: an in-flight miss
    /// may be about to publish the very data that was invalidated.
    pub async fn invalidate_tag(&self, tag: &str) {
        let mut inner = self.inner.write().await;
        inner.generation += 1;
        let epoch = inner.generation;
        inner.invalidated_tags.insert(tag.to_string(), epoch);
        let hashes = inner.tag_index.get(tag).cloned().unwrap_or_default();
        for hash in hashes {
            inner.invalidated_keys.insert(hash.clone(), epoch);
            inner.in_flight.remove(&hash);
            if let Some(entry) = inner.remove_entry(&hash) {
                self.remove_files(&hash, entry.tier);
            }
        }
    }

    pub async fn invalidate_key(&self, key: &str) {
        let hash = hash_key(key);
        let mut inner = self.inner.write().await;
        inner.generation += 1;
        let epoch = inner.generation;
        inner.invalidated_keys.insert(hash.clone(), epoch);
        inner.in_flight.remove(&hash);
        if let Some(entry) = inner.remove_entry(&hash) {
            self.remove_files(&hash, entry.tier);
        }
    }

    /// Clear is a publication barrier too. Do not release the lock before
    /// deleting files, or a new request could lose its just-published entry.
    pub async fn clear(&self) {
        let mut inner = self.inner.write().await;
        inner.generation += 1;
        inner.cleared_at = inner.generation;
        inner.index.clear();
        inner.tag_index.clear();
        inner.total_disk_usage = 0;
        inner.access_counter = 0;
        inner.in_flight.clear();
        inner.invalidated_keys.clear();
        inner.invalidated_tags.clear();
        for tier in &ALL_TIERS {
            if let Ok(entries) = fs::read_dir(self.base_dir.join(tier.subdir())) {
                for entry in entries.flatten() {
                    fs::remove_file(entry.path()).ok();
                }
            }
        }
    }

    pub async fn begin_refresh(&self, key: &str, ticket: &FetchTicket) -> Option<RefreshTicket> {
        let hash = hash_key(key);
        let mut inner = self.inner.write().await;
        if !inner.is_current(ticket, &hash, &[]) || inner.in_flight.contains_key(&hash) {
            return None;
        }
        inner.next_refresh_id += 1;
        let id = inner.next_refresh_id;
        inner.in_flight.insert(hash.clone(), id);
        Some(RefreshTicket { hash, id })
    }

    pub async fn finish_refresh(&self, ticket: &RefreshTicket) {
        let mut inner = self.inner.write().await;
        if inner.in_flight.get(&ticket.hash) == Some(&ticket.id) {
            inner.in_flight.remove(&ticket.hash);
        }
    }

    /// Check if enough time has passed since last refresh attempt.
    /// Returns true if we should retry (no recent attempt OR >min_interval elapsed).
    pub async fn should_retry_refresh(&self, key: &str, min_interval_secs: u64) -> bool {
        let hash = hash_key(key);
        let inner = self.inner.read().await;

        if let Some(entry) = inner.index.get(&hash) {
            if let Some(last_attempt) = entry.last_refresh_attempt {
                let elapsed = now_secs().saturating_sub(last_attempt);
                elapsed >= min_interval_secs
            } else {
                true // Never attempted
            }
        } else {
            true // Entry doesn't exist
        }
    }

    /// Mark that we're attempting a refresh (updates timestamp).
    pub async fn mark_refresh_attempt(&self, ticket: &RefreshTicket) {
        let mut inner = self.inner.write().await;
        if inner.in_flight.get(&ticket.hash) != Some(&ticket.id) {
            return;
        }
        if let Some(entry) = inner.index.get_mut(&ticket.hash) {
            entry.last_refresh_attempt = Some(now_secs());
        }
    }

    /// Get cache statistics for monitoring/debugging.
    pub async fn stats(&self) -> CacheStats {
        let inner = self.inner.read().await;

        let mut by_tier: HashMap<String, TierStats> = HashMap::new();
        for entry in inner.index.values() {
            let tier_name = format!("{:?}", entry.tier);
            let stats = by_tier.entry(tier_name).or_insert(TierStats {
                count: 0,
                size_mb: 0.0,
            });
            stats.count += 1;
            stats.size_mb += entry.size as f64 / (1024.0 * 1024.0);
        }

        let total_disk_mb = inner.total_disk_usage as f64 / (1024.0 * 1024.0);
        let max_disk_mb = MAX_DISK_BYTES as f64 / (1024.0 * 1024.0);

        CacheStats {
            total_entries: inner.index.len(),
            total_disk_mb,
            max_disk_mb,
            usage_percent: (total_disk_mb / max_disk_mb) * 100.0,
            by_tier,
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn remove_files(&self, hash: &str, tier: CacheTier) {
        let dir = self.base_dir.join(tier.subdir());
        fs::remove_file(dir.join(format!("{}.dat", hash))).ok();
        fs::remove_file(dir.join(format!("{}.meta", hash))).ok();
    }

    async fn maybe_evict(&self) {
        let usage = {
            let inner = self.inner.read().await;
            inner.total_disk_usage
        };

        if usage <= MAX_DISK_BYTES {
            return;
        }

        log::info!(
            "[DiskCache] evicting: {:.1} MB > {:.1} MB limit",
            usage as f64 / (1024.0 * 1024.0),
            MAX_DISK_BYTES as f64 / (1024.0 * 1024.0)
        );

        let mut inner = self.inner.write().await;

        // Collect entries sorted by last_access ascending (oldest first).
        let mut entries: Vec<(String, u64, u64, CacheTier)> = inner
            .index
            .iter()
            .map(|(hash, e)| (hash.clone(), e.last_access, e.size, e.tier))
            .collect();
        entries.sort_by_key(|(_, access, _, _)| *access);

        for (hash, _, size, tier) in entries {
            if inner.total_disk_usage <= EVICT_TARGET {
                break;
            }
            self.remove_files(&hash, tier);
            inner.remove_entry(&hash);
            log::debug!(
                "[DiskCache] evicted {} ({} bytes, {:?})",
                &hash[..12],
                size,
                tier
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Key hashing
// ---------------------------------------------------------------------------

fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> (tempfile::TempDir, DiskCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(dir.path(), Arc::new(Crypto::for_tests()));
        (dir, cache)
    }

    async fn age_entry(cache: &DiskCache, key: &str, age: u64) {
        cache
            .inner
            .write()
            .await
            .index
            .get_mut(&hash_key(key))
            .unwrap()
            .created_at = now_secs() - age;
    }

    #[tokio::test]
    async fn late_miss_cannot_repopulate_after_clear_key_or_empty_tag_invalidation() {
        for invalidation in ["clear", "key", "tag"] {
            let (dir, cache) = cache();
            let cache = Arc::new(cache);
            let ticket = cache.begin_fetch().await;
            let (release, waiting) = tokio::sync::oneshot::channel();
            let worker_cache = cache.clone();
            let worker = tokio::spawn(async move {
                waiting.await.unwrap();
                worker_cache
                    .put_if_current(
                        &ticket,
                        "pending",
                        b"old",
                        CacheTier::UserContent,
                        &["favorites"],
                    )
                    .await
                    .unwrap()
            });
            match invalidation {
                "clear" => cache.clear().await,
                "key" => cache.invalidate_key("pending").await,
                _ => cache.invalidate_tag("favorites").await,
            }
            release.send(()).unwrap();
            assert!(
                !worker.await.unwrap(),
                "late {invalidation} publication succeeded"
            );
            assert!(matches!(
                cache.get("pending", CacheTier::UserContent).await,
                CacheResult::Miss
            ));
            drop(cache);
            let reopened = DiskCache::new(dir.path(), Arc::new(Crypto::for_tests()));
            assert_eq!(reopened.stats().await.total_entries, 0);
        }
    }

    #[tokio::test]
    async fn invalidation_fences_only_affected_tags_and_new_requests_can_publish() {
        let (_dir, cache) = cache();
        let old = cache.begin_fetch().await;
        cache.invalidate_tag("favorites").await;
        assert!(!cache
            .put_if_current(
                &old,
                "favorites:1",
                b"old",
                CacheTier::UserContent,
                &["favorites"]
            )
            .await
            .unwrap());
        assert!(cache
            .put_if_current(&old, "cover", b"image", CacheTier::Image, &["image"])
            .await
            .unwrap());
        let current = cache.begin_fetch().await;
        assert!(cache
            .put_if_current(
                &current,
                "favorites:1",
                b"new",
                CacheTier::UserContent,
                &["favorites"]
            )
            .await
            .unwrap());
        assert!(
            matches!(cache.get("favorites:1", CacheTier::UserContent).await, CacheResult::Fresh(data) if data == b"new")
        );
        assert!(
            matches!(cache.get("cover", CacheTier::Image).await, CacheResult::Fresh(data) if data == b"image")
        );
    }

    #[tokio::test]
    async fn an_old_refresh_cannot_release_its_replacement_or_start_after_invalidation() {
        let (_dir, cache) = cache();
        cache
            .put("feed", b"stale", CacheTier::UserContent, &["feed"])
            .await
            .unwrap();
        let old_ticket = cache.begin_fetch().await;
        let old = cache.begin_refresh("feed", &old_ticket).await.unwrap();
        assert!(cache.begin_refresh("feed", &old_ticket).await.is_none());
        cache.invalidate_tag("feed").await;
        assert!(cache.begin_refresh("feed", &old_ticket).await.is_none());
        cache
            .put("feed", b"replacement", CacheTier::UserContent, &["feed"])
            .await
            .unwrap();
        let current_ticket = cache.begin_fetch().await;
        let current = cache.begin_refresh("feed", &current_ticket).await.unwrap();
        cache.mark_refresh_attempt(&old).await;
        assert!(cache.should_retry_refresh("feed", 300).await);
        cache.finish_refresh(&old).await;
        assert!(cache.begin_refresh("feed", &current_ticket).await.is_none());
        cache.mark_refresh_attempt(&current).await;
        assert!(!cache.should_retry_refresh("feed", 300).await);
        cache.finish_refresh(&current).await;
        assert!(cache.begin_refresh("feed", &current_ticket).await.is_some());
    }

    #[tokio::test]
    async fn old_reader_cleanup_cannot_remove_a_newly_published_entry() {
        let (_dir, cache) = cache();
        cache
            .put("album", b"old", CacheTier::StaticMeta, &["album"])
            .await
            .unwrap();
        let old_version = cache.inner.read().await.index[&hash_key("album")].version;
        cache
            .put("album", b"new", CacheTier::StaticMeta, &["album"])
            .await
            .unwrap();
        cache
            .remove_version(&hash_key("album"), CacheTier::StaticMeta, old_version)
            .await;
        assert!(
            matches!(cache.get("album", CacheTier::StaticMeta).await, CacheResult::Fresh(data) if data == b"new")
        );
    }

    #[tokio::test]
    async fn publication_and_invalidation_share_one_commit_boundary() {
        let (dir, cache) = cache();
        let ticket = cache.begin_fetch().await;
        let (write, ()) = tokio::join!(
            cache.put_if_current(&ticket, "album", b"old", CacheTier::StaticMeta, &["album"]),
            cache.invalidate_tag("album"),
        );
        write.unwrap();
        assert!(matches!(
            cache.get("album", CacheTier::StaticMeta).await,
            CacheResult::Miss
        ));
        drop(cache);
        let reopened = DiskCache::new(dir.path(), Arc::new(Crypto::for_tests()));
        assert_eq!(reopened.stats().await.total_entries, 0);
    }

    #[tokio::test]
    async fn overwriting_updates_only_the_entries_own_tags_and_size() {
        let (_dir, cache) = cache();
        cache
            .put("cover", b"image", CacheTier::Image, &["image"])
            .await
            .unwrap();
        cache
            .put("album", b"old", CacheTier::StaticMeta, &["old", "shared"])
            .await
            .unwrap();
        cache
            .put(
                "album",
                b"replacement",
                CacheTier::StaticMeta,
                &["new", "shared"],
            )
            .await
            .unwrap();

        let inner = cache.inner.read().await;
        assert_eq!(inner.index.len(), 2);
        assert_eq!(inner.total_disk_usage, 16);
        assert!(!inner.tag_index.contains_key("old"));
        assert_eq!(inner.tag_index["image"], HashSet::from([hash_key("cover")]));
        assert_eq!(inner.tag_index["new"], HashSet::from([hash_key("album")]));
        assert_eq!(
            inner.tag_index["shared"],
            HashSet::from([hash_key("album")])
        );
        drop(inner);
        assert!(
            matches!(cache.get("album", CacheTier::StaticMeta).await, CacheResult::Fresh(data) if data == b"replacement")
        );
    }

    #[tokio::test]
    async fn invalidating_a_tag_removes_every_member_without_touching_unrelated_entries() {
        let (_dir, cache) = cache();
        cache
            .put("a", b"aa", CacheTier::UserContent, &["favorites", "user:1"])
            .await
            .unwrap();
        cache
            .put(
                "b",
                b"bbb",
                CacheTier::UserContent,
                &["favorites", "user:2"],
            )
            .await
            .unwrap();
        cache
            .put("cover", b"image", CacheTier::Image, &["image"])
            .await
            .unwrap();
        cache.invalidate_tag("favorites").await;
        assert!(matches!(
            cache.get("a", CacheTier::UserContent).await,
            CacheResult::Miss
        ));
        assert!(matches!(
            cache.get("b", CacheTier::UserContent).await,
            CacheResult::Miss
        ));
        assert!(
            matches!(cache.get("cover", CacheTier::Image).await, CacheResult::Fresh(data) if data == b"image")
        );
        let inner = cache.inner.read().await;
        assert_eq!(inner.total_disk_usage, 5);
        assert_eq!(inner.tag_index.len(), 1);
        assert_eq!(inner.tag_index["image"].len(), 1);
        drop(inner);
        cache.invalidate_tag("missing").await;
        cache.invalidate_key("cover").await;
        let inner = cache.inner.read().await;
        assert!(inner.index.is_empty());
        assert!(inner.tag_index.is_empty());
        assert_eq!(inner.total_disk_usage, 0);
    }

    #[tokio::test]
    async fn restart_rebuilds_the_same_tag_membership_from_unchanged_metadata() {
        let (dir, cache) = cache();
        cache
            .put(
                "cover",
                b"image",
                CacheTier::Image,
                &["image", "image", "album:1"],
            )
            .await
            .unwrap();
        cache
            .put("album", b"metadata", CacheTier::StaticMeta, &["album:1"])
            .await
            .unwrap();
        drop(cache);
        let cache = DiskCache::new(dir.path(), Arc::new(Crypto::for_tests()));
        let inner = cache.inner.read().await;
        assert_eq!(inner.index.len(), 2);
        assert_eq!(inner.tag_index["image"].len(), 1);
        assert_eq!(inner.tag_index["album:1"].len(), 2);
        drop(inner);
        cache.invalidate_tag("album:1").await;
        assert_eq!(cache.stats().await.total_entries, 0);
        assert!(cache.inner.read().await.tag_index.is_empty());
        drop(cache);
        let cache = DiskCache::new(dir.path(), Arc::new(Crypto::for_tests()));
        assert_eq!(cache.stats().await.total_entries, 0);
    }

    #[tokio::test]
    async fn cache_freshness_keeps_the_existing_ttl_and_stale_grace() {
        let (_dir, cache) = cache();
        for tier in ALL_TIERS {
            let key = format!("entry:{tier:?}");
            cache.put(&key, b"data", tier, &["entry"]).await.unwrap();
            assert!(
                matches!(cache.get(&key, tier).await, CacheResult::Fresh(data) if data == b"data")
            );
            age_entry(&cache, &key, tier.ttl().as_secs() + 5).await;
            assert!(
                matches!(cache.get(&key, tier).await, CacheResult::Stale(data) if data == b"data")
            );
            age_entry(
                &cache,
                &key,
                tier.ttl().as_secs() + tier.swr_grace().as_secs() + 5,
            )
            .await;
            assert!(matches!(cache.get(&key, tier).await, CacheResult::Miss));
            assert!(!cache.inner.read().await.index.contains_key(&hash_key(&key)));
        }
        assert_eq!(cache.stats().await.total_entries, 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn expired_entries_are_discarded_without_opening_the_data_file() {
        use std::ffi::CString;
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::mpsc;

        let (_dir, cache) = cache();
        let tier = CacheTier::Image;
        cache
            .put("expired", b"old", tier, &["image"])
            .await
            .unwrap();
        age_entry(
            &cache,
            "expired",
            tier.ttl().as_secs() + tier.swr_grace().as_secs() + 5,
        )
        .await;
        let dat = cache
            .base_dir
            .join(tier.subdir())
            .join(format!("{}.dat", hash_key("expired")));
        fs::remove_file(&dat).unwrap();
        let path = CString::new(dat.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a live, NUL-terminated path inside our temporary dir.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&dat)
            .unwrap();
        // Reading this FIFO would block. The watchdog releases an accidental
        // reader, so a regression fails instead of hanging the test process.
        let (done, wait) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            let read_blocked = wait.recv_timeout(Duration::from_secs(5)).is_err();
            if read_blocked {
                writer.write_all(b"old").unwrap();
            }
            read_blocked
        });
        let result = cache.get("expired", tier).await;
        let _ = done.send(());
        assert!(
            !watchdog.join().unwrap(),
            "expired cache tried reading its data file"
        );
        assert!(matches!(result, CacheResult::Miss));
        assert!(!dat.exists());
        assert_eq!(cache.stats().await.total_entries, 0);
    }
}
