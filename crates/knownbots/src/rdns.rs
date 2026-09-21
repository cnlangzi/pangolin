//! Persistent IP → hostname cache for successful RDNS lookups.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::RwLock;

/// Copy-on-write style map behind an `RwLock`. Reads are the common
/// case; writes (a handful of new IPs per day) clone + replace.
pub struct RdnsCache {
    path: PathBuf,
    map: RwLock<Arc<HashMap<String, String>>>,
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
    }

    /// Drop entries whose hostname no longer matches `domains`.
    pub fn prune(&self, domains: &[String]) {
        let mut guard = self.map.write();
        let mut next = HashMap::new();
        for (ip, host) in guard.iter() {
            if match_domain(host, domains) {
                next.insert(ip.clone(), host.clone());
            }
        }
        *guard = Arc::new(next);
    }

    pub fn persist(&self) -> Result<()> {
        let map = Arc::clone(&self.map.read());
        let mut body = String::new();
        for (ip, host) in map.iter() {
            body.push_str(ip);
            body.push(' ');
            body.push_str(host);
            body.push('\n');
        }
        std::fs::write(&self.path, body)
            .with_context(|| format!("persist rdns cache {}", self.path.display()))?;
        Ok(())
    }

    pub fn size(&self) -> usize {
        self.map.read().len()
    }
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
    fn persist_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdns.txt");
        let cache = RdnsCache::open(&path).unwrap();
        cache.set("1.2.3.4", "a.example.com");
        cache.persist().unwrap();
        let cache2 = RdnsCache::open(&path).unwrap();
        assert_eq!(cache2.get("1.2.3.4").as_deref(), Some("a.example.com"));
    }
}
