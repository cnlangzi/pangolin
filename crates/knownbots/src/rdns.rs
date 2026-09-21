//! Persistent IP → hostname cache for successful RDNS lookups.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use parking_lot::RwLock;

/// Copy-on-write style map behind an `RwLock`. Reads are the common
/// case; writes (a handful of new IPs per day) clone + replace.
///
/// Disk persistence is **dirty-flagged**: [`Self::set`] only updates
/// memory; [`Self::persist_if_dirty`] (called from the background
/// scheduler) writes the file. That avoids a full rewrite on every
/// cold RDNS success under a crawl burst.
pub struct RdnsCache {
    path: PathBuf,
    map: RwLock<Arc<HashMap<String, String>>>,
    dirty: AtomicBool,
}

impl RdnsCache {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let map = load_file(&path)?;
        Ok(Self {
            path,
            map: RwLock::new(Arc::new(map)),
            dirty: AtomicBool::new(false),
        })
    }

    pub fn get(&self, ip: &str) -> Option<String> {
        self.map.read().get(ip).cloned()
    }

    pub fn set(&self, ip: &str, hostname: &str) {
        let mut guard = self.map.write();
        if guard.contains_key(ip) {
            return;
        }
        let mut next = (**guard).clone();
        next.insert(ip.to_string(), hostname.to_string());
        *guard = Arc::new(next);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Drop entries whose hostname no longer matches `domains`.
    pub fn prune(&self, domains: &[String]) {
        let mut guard = self.map.write();
        let before = guard.len();
        let mut next = HashMap::new();
        for (ip, host) in guard.iter() {
            if match_domain(host, domains) {
                next.insert(ip.clone(), host.clone());
            }
        }
        let changed = next.len() != before;
        *guard = Arc::new(next);
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Write the cache to disk if [`Self::set`] / [`Self::prune`]
    /// marked it dirty since the last successful persist.
    ///
    /// The dirty flag is cleared **before** the snapshot is taken,
    /// under the same write lock as the map. A `set` that lands
    /// after the snapshot re-dirties the cache, so the next flush
    /// picks it up. Clearing the flag *after* the write would drop
    /// that concurrent `set` (the new entry is in memory, the flag
    /// is then forced false, and a crash before the next `set`
    /// loses it). A failed write puts the flag back.
    pub fn persist_if_dirty(&self) -> Result<()> {
        let snapshot = {
            let guard = self.map.write();
            if !self.dirty.swap(false, Ordering::AcqRel) {
                return Ok(());
            }
            Arc::clone(&guard)
        };
        if let Err(e) = write_map(&self.path, &snapshot) {
            self.dirty.store(true, Ordering::Release);
            return Err(e);
        }
        Ok(())
    }

    pub fn persist(&self) -> Result<()> {
        let map = Arc::clone(&self.map.read());
        write_map(&self.path, &map)
    }

    pub fn size(&self) -> usize {
        self.map.read().len()
    }
}

fn write_map(path: &Path, map: &HashMap<String, String>) -> Result<()> {
    let mut body = String::new();
    for (ip, host) in map.iter() {
        body.push_str(ip);
        body.push(' ');
        body.push_str(host);
        body.push('\n');
    }
    std::fs::write(path, body).with_context(|| format!("persist rdns cache {}", path.display()))?;
    Ok(())
}

fn load_file(path: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let Ok(data) = std::fs::read_to_string(path) else {
        return Ok(map);
    };
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((ip, host)) = line.split_once(' ') {
            let ip = ip.trim();
            let host = host.trim().trim_end_matches('.');
            if !ip.is_empty() && !host.is_empty() {
                map.insert(ip.to_string(), host.to_string());
            }
        }
    }
    Ok(map)
}

/// Hostname equals a domain, or is a subdomain of it.
pub fn match_domain(hostname: &str, domains: &[String]) -> bool {
    let host = hostname.trim_end_matches('.').to_ascii_lowercase();
    for d in domains {
        let d = d.trim_end_matches('.').to_ascii_lowercase();
        if host == d || host.ends_with(&format!(".{d}")) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_domain_suffix() {
        assert!(match_domain(
            "crawl-66-249-66-1.googlebot.com",
            &["googlebot.com".into()]
        ));
        assert!(!match_domain(
            "evil-googlebot.com",
            &["googlebot.com".into()]
        ));
    }

    #[test]
    fn persist_if_dirty_skips_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdns.txt");
        let cache = RdnsCache::open(&path).unwrap();
        // No set → nothing to write; file stays absent.
        cache.persist_if_dirty().unwrap();
        assert!(!path.exists());
        cache.set("1.2.3.4", "a.example.com");
        cache.persist_if_dirty().unwrap();
        assert!(path.exists());
        // Second call is a no-op (dirty cleared).
        std::fs::remove_file(&path).unwrap();
        cache.persist_if_dirty().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn persist_failure_keeps_dirty_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdns.txt");
        let cache = RdnsCache::open(&path).unwrap();
        cache.set("1.2.3.4", "a.example.com");

        use std::os::unix::fs::PermissionsExt;
        let original = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        let err = cache.persist_if_dirty().unwrap_err();
        assert!(err.to_string().contains("rdns"));

        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(original);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        cache.persist_if_dirty().unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("1.2.3.4 a.example.com"));
    }
}
