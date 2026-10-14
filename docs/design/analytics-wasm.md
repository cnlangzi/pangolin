# WASM Analytics Side-Channel

Plug-in, runtime-loadable analytics handlers for `pangolin-ngx`. Operators can
ship a `.wasm` blob (written in AssemblyScript / Rust / TinyGo / any wasm
target) and have it receive every request's `AccessLogEntry` as it finishes,
**without restarting the proxy and without writing Rust**.

The hot path still ends in one line — `App::push_access_log` calls
`AnalyticsHub::enqueue`, which fans out to per-plugin bounded channels.
Each plugin runs in its own worker task that owns a `wasmtime` instance;
fuel metering, memory caps, and call timeouts ensure a misbehaving plugin
cannot stall or kill the proxy.

## Goals

| Goal | Notes |
| ---- | ----- |
| Operators can ship new analytics without touching the proxy binary | Drop a `.wasm` file via admin UI; no Rust change, no restart |
| Hot-path latency stays flat | `push_access_log` adds `N × try_send`, where `N` is the enabled plugin count. ~50 ns each. |
| One bad plugin does not affect the proxy or other plugins | Per-plugin worker task, per-plugin `Store`, per-plugin fuel + memory budget, per-plugin error quarantine |
| Plugins can be replaced / removed / reloaded at runtime | New `id` installed first; old worker's queue drains or times out; `Sender` dropped, worker exits |
| Operators can see what each plugin is doing | Per-plugin call / drop / error / latency counters; captured `log()` output ring buffer |

## Non-goals (v1)

- **Response body sampling.** Plugins only see the metadata; body capture
  is its own design and has privacy + size implications.
- **Header access.** The entry has `host` but not arbitrary request headers.
  Add later if a use case demands it.
- **Component Model / `wit` bindings.** v1 uses a plain JSON ABI so any wasm
  target works. `wit-bindgen` is a v2.
- **Plugin signature verification.** Manifest trust is based on filesystem
  perms + admin session. Cryptographic signing is a v2.
- **Plugin → network egress.** No socket host functions in v1. Plugins that
  need to ship metrics do it via `emit_metric()`, which lands in a local
  buffer the operator can scrape.
- **Persistent plugin state across restarts.** Plugins can keep in-memory
  state (their linear memory survives for the lifetime of their worker).
  Restarting the proxy starts every plugin from zero.

## Architecture

```
                    request_filter            response_filter                 logging
                         │                          │                            │
                         ▼                          ▼                            ▼
                 ctx.start = now       App::push_access_log(entry)        traffic.on_finish
                                            │
                  ┌─────────────┬───────────┼──────────────┬──────────────────┐
                  │             │           │              │                  │
                  ▼             ▼           ▼              ▼                  ▼
          access_log_recent  access_   bot_*  sinks    AnalyticsHub      (existing)
                            log_tx        (verified       │
                                          UA only)       │
                                                         │  per-plugin
                                                         │  mpsc::Sender
                                                         │  (bounded)
                                                         ▼
                       ┌──────────┐    ┌──────────┐    ┌──────────┐
                       │ worker A │    │ worker B │    │ worker C │
                       │ anomaly  │    │  geoip   │    │ rate-calc│
                       │   .wasm  │    │  .wasm   │    │  .wasm   │
                       └──────────┘    └──────────┘    └──────────┘
                            │               │               │
                            ▼               ▼               ▼
                        host funcs    host funcs        host funcs
                        (log, now,    (log, now,        (log, now,
                         emit_metric) emit_metric)      emit_metric)
                            │               │               │
                            ▼               ▼               ▼
                       CapturedLog   CapturedLog     CapturedLog
                       (per-plugin   ring buffer    ring buffer
                        ring, last                (admin /api/.../logs)
                        200 lines)
```

The hot path does not call into wasm. It only does `try_send` to each
plugin's bounded channel; a full channel drops the sample for that plugin
only and increments a per-plugin `dropped` counter. The proxy is unaffected.

## Crate layout

New crate `crates/analytics/`, **independent of `pangolin-core`**, to keep
`wasmtime`'s cold-compile time out of the core crate (which is linked by
tests, the `tun` binary, and unit tests). `pangolin-core` depends on
`analytics` (the other way around would put `wasmtime` into every binary
that links `pangolin-core`).

```
crates/analytics/
├── Cargo.toml        # wasmtime, serde, tokio, parking_lot, anyhow
├── src/
│   ├── manifest.rs   # PluginManifest: name/version/config/filter
│   ├── abi.rs        # host functions: log, now_ms, emit_metric
│   ├── plugin.rs     # PluginInstance: Module, fuel budget, stats
│   ├── worker.rs     # one worker task per plugin; recv → call
│   ├── hub.rs        # AnalyticsHub: enqueue / register / unregister / reload
│   └── filter.rs     # AccessLogFilter (host_glob, status_min, method_set)
```

`pangolin-core/src/app.rs::push_access_log` grows by exactly one line:

```rust
pub fn push_access_log(&self, entry: AccessLogEntry) {
    self.access_log_recent.push(entry.clone());
    let _ = self.access_log_tx.send(entry.clone());
    // ... existing bot_* fan-out ...

    // NEW: wasm analytics fan-out
    self.analytics.enqueue(entry);
}
```

`proxy.rs` is untouched.

## Plugin ABI (v1)

**Plain JSON-in / int-return**, deliberately the simplest ABI that works.

```wat
(module
  (import "env" "log"          (func $log (param i32 i32)))
  (import "env" "now_ms"       (func $now (result i64)))
  (import "env" "emit_metric"  (func $emit (param i32 i32) (param f64)))
  (memory (export "memory") 1)
  (export "handle" (func $handle))
  (func $handle (param $ptr i32) (param $len i32) (result i32)
    ;; host has written `len` bytes of UTF-8 JSON starting at offset `ptr`
    ;; in this module's linear memory.
    ;; Return 0 on success; non-zero is recorded as an error.
    i32.const 0
  )
)
```

### Input contract

`ptr..ptr+len` contains one `AccessLogEntry` serialised as JSON. Shape
mirrors `crates/pangolin-core/src/events.rs::AccessLogEntry`:

```json
{
  "timestamp":   "2026-10-14T12:34:56.789Z",
  "method":      "GET",
  "path":        "/api/widgets",
  "host":        "widgets.example.com",
  "status":      200,
  "duration_ms": 12,
  "backend":     "direct:10.0.0.7:8080",
  "client_ip":   "203.0.113.42",
  "user_agent":  "curl/8.0"
}
```

The buffer is owned by the host; the plugin must not retain the pointer
across calls. Plugins that need to keep data copy it into their own memory.

### Return contract

- `0` — success
- non-zero — error; the worker logs `plugin <id> returned <code>`, increments
  `error_count`, and applies the quarantine policy if the error budget is
  exhausted. The host does not interpret the code.

### Host functions (v1)

| Symbol | Signature | Semantics |
| ------ | --------- | --------- |
| `log` | `(level: i32, ptr: i32, len: i32) -> ()` | `level`: 0=trace, 1=debug, 2=info, 3=warn, 4=error. Captured to per-plugin ring buffer (last 200 lines). |
| `now_ms` | `() -> i64` | Monotonic milliseconds since plugin start. Wall clock (`Utc::now`) is not exposed to keep plugin timing deterministic. |
| `emit_metric` | `(name_ptr: i32, name_len: i32, value: f64) -> ()` | Records a labelled metric to the plugin's in-memory metric buffer. Exposed via `GET /api/analytics/plugins/{id}/metrics`. |

**Not exposed in v1**: filesystem, network sockets, environment variables,
wall-clock time, random number generator. Plugins that need randomness can
hash `now_ms()` and the entry bytes.

## Plugin manifest

Lives next to the `.wasm` blob. Same stem, `.manifest.json` suffix.
`anomaly.wasm` ↔ `anomaly.manifest.json`.

```json
{
  "name": "anomaly-detector",
  "version": "1.0.0",
  "description": "flags requests with duration > p99 * 3",
  "queue_size": 2048,
  "fuel_per_call": 500000,
  "memory_bytes": 8388608,
  "call_timeout_ms": 30,
  "filter": {
    "host_glob": ["*.example.com"],
    "status_min": 500
  }
}
```

| Field | Default | Meaning |
| ----- | ------- | ------- |
| `name` | required | Plugin id. Unique. Becomes the admin URL slug. |
| `version` | `"0.0.0"` | Free-form; displayed in admin UI. |
| `description` | `""` | Free-form; displayed in admin UI. |
| `queue_size` | `analytics.queue_size` | Per-plugin bounded channel depth. |
| `fuel_per_call` | `analytics.fuel_per_call` | wasmtime fuel budget per call. |
| `memory_bytes` | `analytics.memory_bytes` | wasmtime `Store` memory cap. |
| `call_timeout_ms` | `analytics.call_timeout_ms` | Hard wall-clock cap on one call. |
| `filter.host_glob` | `[]` (match all) | Glob match on `AccessLogEntry.host` (after lowercase + port strip). |
| `filter.status_min` | `null` | Skip entries with `status < N`. |
| `filter.status_max` | `null` | Skip entries with `status > N`. |
| `filter.method_set` | `[]` (match all) | Whitelist of methods (e.g. `["GET","POST"]`). |

`host_glob` matches using `pangolin_core::is_valid_domain` semantics plus
a `*` wildcard in the leading label (`*.example.com` matches
`foo.example.com`, `bar.foo.example.com`, etc.). Exact hosts match exactly.

## Plugin lifecycle

```
  register                                  unload (admin or quarantine)
       │                                              │
       ▼                                              ▼
  load .wasm ─► compile Module ─► alloc worker task ─► (recv loop) ─► Sender drop ─► task exits
                     │                                                       │
                     ▼                                                       ▼
                store on disk                                              metrics persist
                under Module cache                                          until unload
```

### Register

1. Admin posts multipart `wasm` + `manifest` to `/api/analytics/plugins/load`,
   **or** the operator copies files into `analytics.dir` and the periodic
   scanner (every 5s) picks them up (opt-in via `analytics.watch_dir`).
2. Hub parses the manifest. Rejects if `name` collides with a registered
   plugin unless the existing plugin has the same `version` *and* the
   operator confirmed replace (POST `/reload`).
3. Compile `Module` once (cache by SHA256 of the `.wasm` blob).
4. Spawn the worker task. Worker allocates the first `Store` lazily on the
   first `recv()`, not at registration time — registration is O(file size)
   compile + a `HashMap` insert, not a wasmtime instance.

### Hot reload

POST `/api/analytics/plugins/{id}/reload`:

1. Compile new `Module`. If compilation fails, the old plugin keeps running.
2. Spawn a new worker task under a new internal `PluginInstance`. Both old
   and new workers receive calls for a **short overlap window** (default 5 s)
   so in-flight entries aren't lost. The admin UI marks the slot as
   `reloading` during the overlap.
3. After the overlap, the old `Sender` is dropped; the old worker exits
   naturally once its remaining queue drains or after a hard kill at the
   overlap deadline.

### Unload

DELETE `/api/analytics/plugins/{id}`:

1. Drop the `Sender`. The worker drains its queue, then exits on
   `Disconnected`.
2. `HashMap` entry removed. Plugin metrics retained for one admin page-load
   so the operator can see "what did this plugin do".

### Auto-quarantine

If a plugin errors (returns non-zero, traps on fuel exhaustion, OOMs, or
exceeds `call_timeout_ms`) `auto_quarantine_threshold` times in a row
(default 10), the hub:

1. Marks the plugin `quarantined` (visible in admin).
2. Drops the `Sender` (no more calls reach it).
3. Logs a `warn!` line with the plugin id and last error.
4. Keeps the `.wasm` and manifest on disk; the operator decides whether to
   `reload` (after fixing the plugin) or `delete`.

## Hot path discipline

`AnalyticsHub::enqueue` is the only function called from
`push_access_log`. It:

1. Reads the enabled plugin list under a `parking_lot::RwLock::read`
   (microsecond-scale; never held across an await).
2. For each enabled plugin, runs the `filter` and, if the entry passes,
   `try_send`s the entry onto the plugin's bounded `mpsc::SyncSender`.
3. Returns. No `await`, no allocation in the common case (the entry is
   moved; plugin channels are pre-allocated).

If a plugin's channel is full, the hub increments that plugin's
`dropped_samples` counter and returns. The proxy is unaffected.

Per-plugin worker task loop:

```rust
loop {
    let entry = match rx.recv().await {
        Some(e) => e,
        None => break, // Sender dropped; shutdown
    };

    let mut store = wasmtime::Store::new(&engine, ());
    store.set_fuel(fuel_per_call)?;
    store.limiter(|_| memory_limiter);

    let instance = linker.instantiate(&mut store, &module)?;
    let handle = instance.get_typed_func::<(i32, i32), i32>(&mut store, "handle")?;

    // Write entry bytes into wasm memory at offset 0.
    let json = serde_json::to_vec(&entry)?;
    let ptr = write_to_memory(&mut store, &instance, &json)?;

    match timeout(call_timeout, handle.call(&mut store, (ptr, json.len() as i32))).await {
        Ok(Ok(0)) => stats.success += 1,
        Ok(Ok(code)) => stats.errors += 1; consider_quarantine(),
        Ok(Err(trap)) => stats.traps += 1; consider_quarantine(),
        Err(_) => stats.timeouts += 1; consider_quarantine(),
    }
}
```

Per-call cost (estimated, one plugin, AssemblyScript hello-world):

| Step | Time |
| ---- | ---- |
| `serde_json::to_vec` of a small `AccessLogEntry` | ~2 µs |
| `Store::new` + `linker.instantiate` | ~100–300 µs |
| `write_to_memory` (memcpy into wasm linear memory) | ~1 µs |
| `handle.call` | plugin-dependent (typical: < 50 µs) |
| **Total per call** | **~150–400 µs** |

If profiling later shows instance-per-call is the bottleneck, the worker
holds a `Mutex<Store<()>>` and reuses it across calls. The mutex serialises
calls inside one worker, but **parallelism across plugins is unaffected**.

## Capacity & backpressure

| Cap | Default | On overflow |
| --- | ------- | ----------- |
| Per-plugin channel | 4096 (or `manifest.queue_size`) | Drop the sample, `dropped_samples++` for that plugin |
| Per-call fuel | 100 000 (or `manifest.fuel_per_call`) | Trap → error counter → quarantine |
| Per-call memory | 16 MiB (or `manifest.memory_bytes`) | Trap on next allocation → quarantine |
| Per-call wall clock | 50 ms (or `manifest.call_timeout_ms`) | Forced abort via wasmtime epoch + tokio timeout → quarantine |

`dropped_samples` is reported per-plugin in the admin UI so operators can
see "this plugin is too slow" without grepping logs.

## Admin UI

| Route | Method | Purpose |
| ----- | ------ | ------- |
| `/analytics` | GET | Page; lists plugins, status, counters |
| `/api/analytics/plugins` | GET | HTMX table fragment (refreshes every 5 s) |
| `/api/analytics/plugins/load` | POST | multipart `.wasm` + `.manifest.json` |
| `/api/analytics/plugins/{id}` | DELETE | Unload |
| `/api/analytics/plugins/{id}/reload` | POST | Hot reload |
| `/api/analytics/plugins/{id}/enable` | POST | Re-enable a quarantined plugin |
| `/api/analytics/plugins/{id}/disable` | POST | Disable without removing file |
| `/api/analytics/plugins/{id}/quarantine-clear` | POST | Reset error counter after a fix |
| `/api/analytics/plugins/{id}/logs` | GET | Last 200 `log()` calls, as text/plain (so an HTMX fragment can render them) |
| `/api/analytics/plugins/{id}/metrics` | GET | JSON dump of `emit_metric` calls |

All mutating routes require an admin session and a valid CSRF token.

## Config

```yaml
# ngx.yml — new top-level section
analytics:
  enabled: true                  # master switch; false → no compile, no spawn
  dir: "./analytics"             # watch dir; scanned on boot and every 5 s if watch_dir=true
  watch_dir: false               # opt-in hot load from filesystem; default off (admin API only)
  queue_size: 4096               # per-plugin default channel depth
  fuel_per_call: 100000          # wasmtime fuel per call
  memory_bytes: 16777216         # 16 MiB per plugin
  call_timeout_ms: 50            # hard wall-clock cap on one call
  auto_quarantine_threshold: 10  # consecutive errors → unload
  reload_overlap_secs: 5         # dual-call window during /reload
  scan_interval_secs: 5          # dir scan cadence when watch_dir=true
```

`NGX_ANALYTICS__ENABLED=false ./ngx` disables everything (compile + spawn
are skipped, the hub is a no-op singleton). The proxy builds and runs
without `wasmtime` initialised when `analytics.enabled: false`.

## Failure modes

| Symptom | Cause | Operator action |
| ------- | ----- | --------------- |
| Admin shows `dropped_samples` rising on a plugin | Plugin worker slower than request rate | Raise `queue_size`, or fix the plugin (or `disable` it) |
| Plugin row shows `quarantined` | `auto_quarantine_threshold` consecutive errors | Inspect `/logs` for that plugin, fix the `.wasm`, POST `/quarantine-clear` then `/reload` |
| `/reload` returns 422 | New manifest invalid (bad JSON, name collision, missing field) | Fix manifest and retry; old plugin keeps running |
| `/reload` returns 500 with "module compile failed" | New `.wasm` is malformed or imports a host function we don't expose | Fix the plugin source; old plugin keeps running |
| All plugins suddenly stop receiving entries | `analytics.enabled` flipped to `false` mid-flight, or hub crashed | Check `pangolin.log`; the proxy itself is unaffected |
| Plugin consumes more memory than the cap | Misconfigured `memory_bytes`, or plugin allocates unboundedly in linear memory | Cap kicks in (trap → quarantine); raise `memory_bytes` only after confirming the plugin is legitimate |
| Hot-reload loops a request between old and new worker | Reload overlap window sends to both | Expected; documented as `reloading` state in admin |
| Plugin uses wall-clock features it shouldn't (e.g. tries to import a `clock` we don't expose) | `wasmtime` linker rejects the import | Compile-time rejection; admin sees 500 on `/load` with the missing import listed |

## Implementation order

1. **`crates/analytics` skeleton + `AnalyticsHub::enqueue` (no wasm).**
   Unit-test that `enqueue` does not block, does not allocate in the hot
   path, drops on full. `App::push_access_log` grows by one line. Verify
   no regression in existing `real_e2e_*` and `cargo bench` traffic.

2. **`wasmtime` integration + an AssemblyScript example.** A
   `examples/anomaly.wasm` that just logs every entry's `host`. Unit-test
   that the example round-trips. Confirm the quarantine trigger fires on a
   deliberately-bad wasm (fuel=1 → traps).

3. **Admin UI** for the table + load/unload/reload buttons + logs view.
   Test the routes with the admin auth harness already in
   `tests/src/access_log_e2e.rs`.

4. **Directory watch + auto-quarantine.** Reuses the in-process watcher
   pattern already in `app.rs`. Opt-in via `analytics.watch_dir`.

5. **v2 components**: `wit-bindgen` ABI, header propagation, response body
   sampling, plugin signing. Tracked as separate issues; not in v1.

Each step ships a regression test pinned by name (see `AGENTS.md`).

## References

- `crates/pangolin-core/src/traffic.rs` — `TrafficHub`, the closest
  precedent for "side-channel that never blocks the hot path".
- `crates/pangolin-core/src/bot_log.rs` — async-fan-out pattern
  (`Mutex<VecDeque>` + `Notify` + spawned writer task).
- `crates/pangolin-core/src/app.rs::push_access_log` — single fan-out
  point we extend.
- `docs/design/access-log.md` — bounded ring buffer + SSE broadcast
  pattern reused for the per-plugin `log()` capture.
- `docs/design/traffic.md` — "drop-on-overflow is fine" doctrine.
- `wasmtime::Store`, `wasmtime::Module`, `wasmtime::Linker` — Rust API
  docs for the host embedding.
