//! The database reads run on the blocking pool, not on an async
//! worker (issue #254).
//!
//! `App::db` is a `tokio::sync::Mutex` around a synchronous
//! `rusqlite::Connection`. A route that takes the lock and then runs
//! a query borrows a worker thread from the pool that proxies
//! requests — the query has to run on the blocking pool instead, and
//! the routes do it through `App::with_db`.
//!
//! The guard is a source scan on purpose: the property belongs to
//! the code, not to one request, so a behavioural test could only
//! catch it on the paths it happens to drive.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common::repo_root;

/// The crates whose code talks to the database.
const CRATES: [&str; 4] = ["admin", "api", "ngx", "pangolin-core"];

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// No route takes the database lock on an async worker thread.
///
/// Every query goes through `App::with_db` — or its `with_db_or` /
/// `with_db_res` wrappers — which runs it on the blocking pool. A
/// `app.db.lock()` left in a route would run a synchronous SQLite
/// query on a worker thread: the thread that is supposed to be
/// proxying requests.
///
/// A call inside a `spawn_blocking` is the one exception — the
/// blocking pool may take the lock there. Add it to the allow-list
/// below if a route ever needs one.
#[test]
fn no_route_takes_the_database_lock_on_an_async_worker() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for crate_name in CRATES {
        let dir = root.join("crates").join(crate_name).join("src");
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for path in files {
            let text = fs::read_to_string(&path).expect("read the source file");
            for (i, line) in text.lines().enumerate() {
                // A mention in a comment is not a call site.
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                let code = line.split("//").next().unwrap_or(line);
                if code.contains(".db.lock()") {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.strip_prefix(&root)
                            .expect("the file is under the root")
                            .display(),
                        i + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these call sites run a synchronous query on an async worker:\n{offenders:#?}"
    );
}
