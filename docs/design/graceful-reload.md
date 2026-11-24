# Graceful reload

Replacing the gateway's binary or its configuration without a
downtime window needs four things, and this document describes the
four the gateway has today:

1. **two processes can hold the same port at the same time** —
   `SO_REUSEPORT` on every listener (issue #233),
2. **the operator can tell whether the new configuration is usable
   before the switch** — `pangolin-ngx --check` (issue #233),
3. **the operator can address the running process by its number** —
   the PID file (issue #233),
4. **a handover that replaces the running process without a gap** —
   the supervisor and the drain (issue #234).

For the configuration keys, see
[`configuration.md`](../configuration.md) → `[pid]` and `[reload]`.

## Contents

- [`SO_REUSEPORT` on every listener](#so_reuseport-on-every-listener)
- [`pangolin-ngx --check`](#pangolin-ngx---check)
- [The PID file](#the-pid-file)
- [The handover](#the-handover)
  - [The process model](#the-process-model)
  - [The two roles](#the-two-roles)
  - [The readiness pipe](#the-readiness-pipe)
  - [The three stages of the drain](#the-three-stages-of-the-drain)
  - [What a failed handover leaves behind](#what-a-failed-handover-leaves-behind)
  - [What a handover costs](#what-a-handover-costs)
  - [Changing the binary](#changing-the-binary)
  - [Upgrading from a version without `SO_REUSEPORT`](#upgrading-from-a-version-without-so_reuseport)

---

## `SO_REUSEPORT` on every listener

`SO_REUSEPORT` is a Unix socket option that lets **several sockets
bind the same address and port**. Under Linux's semantics the kernel
spreads the incoming connections over the sockets of the group by the
hash of each connection's 4-tuple, so two processes can serve the
same port at the same time: the outgoing generation drains while the
incoming one already takes its share of the new connections.

The option is a property of the **socket**, and the kernel only lets
the second socket of the group bind when **every** socket of the
group asked for it. A listener that binds without it takes the port
away from the next process that tries — the second generation would
fail with `EADDRINUSE` and the handover would not happen at all.

### The six listeners

The gateway binds six sockets. All of them ask for the option:

| listener | where | bound by |
| --- | --- | --- |
| `addr.http` | the public HTTP port | pingora (`add_tcp_with_settings`) |
| `addr.https` | the public HTTPS port | pingora (`add_tls_with_settings`) |
| `admin.addr` | the admin UI / admin API | pingora (`add_tcp_with_settings`) |
| `api.listen` | the dashboard-facing management API | `crates/api/src/http.rs` |
| `tunnel.addr` | the tunnel's WebSocket/TCP endpoint | `crates/ngx/src/tunnel.rs` |
| `tunnel.addr` (UDP) | the tunnel's QUIC endpoint — the same port number, a separate port space | `crates/ngx/src/tunnel/quic.rs` |

The first three are pingora's: it takes a `TcpSocketOptions` with
`so_reuseport: Some(true)`, so no change to pingora itself was
needed.

The other three are plain sockets the gateway binds itself. They set
the option on the socket **before** the bind and then hand the socket
over — to tokio for the two TCP listeners, to `pangolin_tun` for the
QUIC endpoint (whose `bind_endpoint_with_socket` takes a socket the
caller bound itself).

### Why all six, and not the three that are obvious

The QUIC endpoint is the one that is easy to leave out, and leaving
it out is expensive. QUIC is the tunnel's **primary** transport; when
the gateway cannot bind it, it logs a warning and keeps serving the
WebSocket/TCP fallback — and that downgrade is **persistent**: the
tun keeps using the transport it negotiated, and the sessions that
are up stay on the fallback. A handover that lost the QUIC port would
silently push every tunnel onto the slower transport.

`api.listen` is not optional either: a second generation that cannot
bind the management API is a gateway the dashboard cannot talk to.

### What it does not change

A deployment with **one** gateway is unaffected. With a single
listener in the reuseport group, the kernel's choice has exactly one
candidate, so every connection lands where it landed before.

### The cost

Two sockets on one port: the kernel hashes the connection's 4-tuple
onto one of them. The hash is stable for the life of the connection,
so a connection never migrates between the sockets — but a connection
that was **already up** when the group grew is re-spread, and roughly
half of the sessions that were up lose their packets to the socket
that joined. The tun reconnects (the crate's own client is back in
about a second) and the gateway's requests on those sessions fail.
That is the price of the handover, and it is paid once, during the
switch.

---

## `pangolin-ngx --check`

The check answers one question: **would the gateway start with this
configuration?** It runs the validation steps of the real startup and
stops before anything is bound.

```console
$ pangolin-ngx --config /etc/pangolin/ngx.yml --check
$ echo $?
0
```

A successful check prints **nothing** — neither on stdout nor on
stderr. The exit code is the answer.

| outcome | exit code | stderr |
| --- | --- | --- |
| the configuration is usable | `0` | empty |
| the configuration does not parse | non-zero | the parse error |
| a listen address does not parse | non-zero | the key, named |
| the cert dir is missing | non-zero | the cert dir, named |
| the database cannot be opened | non-zero | the database error |
| the certificates cannot be reconciled | non-zero | the reconciliation error |

### What it does

The steps, in the order the real startup takes them:

1. the rustls crypto provider is installed (the process does this
   before the arguments are parsed — rustls needs it),
2. the configuration is parsed,
3. the **listen addresses** are parsed — the three the gateway
   parses itself (`tunnel.addr`, `api.listen`) and the three
   pingora parses (`addr.http`, `addr.https`, `admin.addr`),
4. the database is opened (`App::new`, which applies the pending
   migrations),
5. the cert dir has to exist and has to be a directory,
6. the certificates on disk are reconciled into the `certs` table,
7. the SNI callback is built.

### What it does not do

It binds **no** port, starts **no** service, installs **no** signal
handler and writes **no** PID file. The check returns in
milliseconds.

### The side effects — it is not a read-only operation

The check opens the database, and opening the database **applies the
pending migrations**. It also resets the certificate rows a dead
process left in `Issuing` (the ACME startup watchdog) and reconciles
the certificates on disk into the `certs` table — both writes.

That is deliberate. A check that took a different path than the real
startup would answer a question nobody asked: "would *this* code
path work?" is not the same as "would the gateway work?". The check
follows the startup, and the side effects are what the startup would
have done anyway. They are documented in `--help` and here.

Two consequences the operator has to know:

- **Run the check from the service's working directory.** The
  database is `pangolin.db` in the **current working directory** —
  the packaged service runs in `/var/lib/pangolin`, the Ansible
  deployment in `/usr/local/pangolin`. A check run from somewhere
  else opens a different (empty) database and says nothing about the
  one the service uses.
- **Do not run the check against a live gateway's database.** The
  check writes to it, and the ACME startup watchdog can abort an
  in-flight DNS-01 issuance. The gateway is meant to be checked
  while it is stopped.

### Where the check is stricter than the startup

The startup only logs a warning when the certificate reconciliation
fails and serves anyway; the check fails. A failed reconciliation
means the disk and the database disagree about the certificates — the
operator wants to hear that **before** a handover, not after it.

### The cert dir

The check **fails** when the cert dir is missing, and it does not
create it. The real startup creates the directory — a fresh
installation has none — but a check that creates what it is about to
complain about is no check at all. The operator creates the directory
(or points `acme.cert_dir` at an existing one) and runs the check
again.

---

## The PID file

The PID file holds one decimal process number and a newline:

```console
$ cat /var/lib/pangolin/pangolin-ngx.pid
4711
```

It is the anchor for the operator commands that address the gateway by
its process number — "is it alive?", "which process is that?" — in
the deployments that do not go through systemd.

### Where it lives

The path comes from the `[pid]` section of `ngx.yml`:

```yaml
pid:
  pid_file: /var/lib/pangolin/pangolin-ngx.pid
```

Without that key the path is derived from the **configuration file's
own path**: `./ngx.yml` → `./ngx.pid`. The same convention the tun
uses for its pin file (`tun.yml` → `tun.pins`), so the side files of
a configuration live next to it.

The packaged deployments name the path explicitly, because their
configuration lives in a directory the service user cannot write to
(`/etc/pangolin/`) while their state directory (`/var/lib/pangolin/`)
is theirs.

### The lifecycle

- The **supervisor** writes the file (issue #234) — after it has
  read the configuration and before it binds any listener. The file
  says "this process intends to serve", not "the ports are up".
- A graceful exit removes it.
- A `SIGKILL` leaves it behind. The next start truncates it, so a
  stale number never survives a restart.

The number in the file is the **supervisor's**: it is the process
systemd's `MAINPID` points at, and the one `pangolin-ngx --reload`
signals.

The removal only happens when the file still carries **this** process's
number. Two generations of the gateway may share one path during a
handover, and the outgoing generation must not delete the incoming
one's file on its way out — the operator's "which process is that?"
would then answer with nothing.

A PID file that cannot be written is **not** a reason to refuse to
start: the gateway logs the error and serves anyway. The PID file is
an anchor for the operator's convenience, not a precondition for the
service.

---

## What is left for the handover

Nothing — the handover is the chapter above. The three pieces before
it are the groundwork it rests on.

Two things are worth saying about them here, because they are the
traps the handover had to avoid:

- the PID file is written **before** the listeners are bound, so it
  cannot be used as a readiness signal. A handover that waits for the
  new generation's PID file would start the traffic too early.
- the QUIC endpoint degrades **silently** when it cannot bind. A
  handover that does not check the QUIC listener's log line would
  push every tunnel onto the WebSocket/TCP fallback without saying
  so.

## The handover

`systemctl reload pangolin-ngx` — or `pangolin-ngx --reload` —
replaces the running gateway with a new one **without a gap in the
service**. The port is held throughout, the in-flight requests are
finished, and a handover that fails leaves the running gateway
exactly as it was.

### The process model

```text
systemd ──MAINPID（恒定）──> supervisor（常驻，不服务流量）
                             └── generation（服务流量，可被替换）
```

The gateway is **two processes**, not one.

The **supervisor** is the resident parent. It is the process
systemd's `MAINPID` points at, it never serves a request, it never
binds a port and it never opens the database. Its whole job is to
start the generations, watch them and replace them. It stays alive
across handovers — which is what makes `ExecReload` work at all: a
parent that exited would leave `$MAINPID` pointing at nothing, the
second `systemctl reload` would fail with "No such process", and
`systemctl stop` would degrade into a 30-second `SIGKILL` of the
whole cgroup.

The **generation** is the child that does the work: it binds the six
listeners, opens the database and serves the traffic. It is the
process that is replaced.

The role is decided by an argument: `pangolin-ngx --generation`
(hidden) enters the generation role, and the supervisor starts every
generation with that argument. The supervisor puts its own process
number in the environment (`PANGLOLIN_SUPERVISOR_PID`), and the
generation checks that the process is really there — a generation
without a supervisor would hold the ports with nobody to replace it.

The kernel kills the generation when its supervisor dies
(`PR_SET_PDEATHSIG`), so a `SIGKILL`ed supervisor does not leave a
generation behind holding the ports.

### The two roles

| | supervisor | generation |
| --- | --- | --- |
| PID file | **writes** it | — |
| Database | never opens it | opens it |
| Listeners | never binds one | binds all six |
| `SIGUSR2` | performs a handover | drains and exits |
| `SIGUSR1` | forwards it to the generation | rebuilds its indexes |
| `SIGTERM` / `SIGINT` | forwards `SIGTERM`, waits, exits 0 | drains and exits |

The PID file holds the **supervisor's** number: `pangolin-ngx
--reload` and `pangolin-ngx --status` address the gateway through
it, and it has to be the number `ExecReload` signals.

The two systemd units need no change: `Type=simple` puts
`MAINPID` on the supervisor, `ExecReload=/bin/kill -SIGUSR2
$MAINPID` now reaches a handler, and `KillMode=mixed` sends the
`SIGTERM` of a stop to the supervisor alone — which forwards it and
waits for the generation. `TimeoutStopSec=30` still holds: the
drain is the budget plus pingora's five seconds.

Locally, `make stop-local-ngx` finds **both** processes
(`pgrep -x pangolin-ngx` matches the supervisor and the generation
alike) and signals both, so the local flow needs nothing either.

### The readiness pipe

The handover cannot rely on the new generation dying to notice that
something is wrong: **pingora binds its own listeners inside its
service thread, and a bind that fails leaves the process alive and
serving nothing.** So the incoming generation has to *say* that it
is ready, and it may only say so once every listener is bound.

The supervisor creates a pipe, hands the **write end** to the
incoming generation through an inherited file descriptor (the number
travels in `PANGLOLIN_RELOAD_READY_FD`), and reads the read end. The
generation writes one byte after the last bind; the descriptor is
closed when the generation goes.

The supervisor waits for that byte for at most
`[reload] ready_timeout_secs` (30 seconds). EOF means the
generation died — a broken configuration, a listener it could not
bind. The timeout means it is alive but not ready. Either way the
handover is abandoned: the incoming generation is killed and **the
running generation never stopped accepting**.

How does the generation know that pingora's listeners are bound? It
looks at the kernel's own view of the socket table:
`/proc/net/tcp` and `/proc/net/tcp6` list the sockets in LISTEN
state, and the socket inodes are matched against `/proc/self/fd` —
the outgoing generation has a socket on the same port, and only the
inode tells the two apart. A port of `0` is skipped: the kernel
picks a free port for it, so the bind cannot collide with anybody.

The QUIC endpoint is different: the gateway binds it itself, so it
knows. And for a generation that has to report itself as ready, a
failed QUIC bind is **fatal** — the first generation of a fresh
start degrades to the WebSocket/TCP fallback and logs a warning (a
network that blocks UDP must not stop the gateway), but a handover
may not: a silent downgrade would push every tunnel onto the slower
transport for good, and the handover would look like it worked.

### The three stages of the drain

The handover runs in the order below. The outgoing generation stops
accepting **only after** the incoming one has said it is ready, so
the port is never without an owner.

```text
supervisor                新 generation              老 generation
    │  fork + execve           │                          │
    ├─────────────────────────>│ 绑定 6 个监听点           │
    │                          │（SO_REUSEPORT，与老代共存）│
    │                          ├─ 写 ready 字节 ─────────>│  supervisor 读到
    │  ── SIGUSR2 ───────────────────────────────────────> │
    │                          │                          │  A 段：停止接受新连接
    │                          │                          │  B 段：关掉 tunnel 会话
    │                          │                          │  C 段：等在途请求
    │                          │                          │  退出
    │  ── SIGUSR1（新代重建索引）─>│
```

**A — stop accepting (immediately).** Every listener stops taking
new connections: the proxy's HTTP and HTTPS ports, the admin port,
the management API's port, the tunnel's TCP listener and the QUIC
endpoint. From this moment every new connection lands on the
incoming generation.

The listeners the gateway binds itself drop their socket when their
accept loop ends, so they leave the `SO_REUSEPORT` group at once.
pingora's three do the same: its accept loop breaks on the shutdown
signal and the listening socket goes with it — which is what makes
the promise true, because a socket that stayed in the group without
accepting would black-hole the connections the kernel spread onto it.

The QUIC endpoint is the one exception: it stops accepting **new
sessions** but keeps its UDP socket until the drain is over, because
the established QUIC sessions live on that socket and the requests
travelling through them have to finish.

**B — close the tunnel sessions (≤ 5 seconds).** The in-flight
tunnel streams get their budget first — a session closed under a live
request would fail that request. Then the sessions are closed: the
QUIC session gets a `CONNECTION_CLOSE` frame, the WebSocket session
has its bridge task aborted. The tun's own reconnect loop (one
second of backoff to start with) then brings the tunnel node back —
to the incoming generation, which is already serving.

**C — finish the in-flight requests (≤ `[reload] drain_timeout_secs`).**
pingora's grace period is the drain budget, so the requests the
generation took when the drain started are finished before it goes.
pingora adds five seconds on top of it to drop its runtimes.

After the outgoing generation is gone, the supervisor sends the
incoming generation a `SIGUSR1`: the writes that landed on the
outgoing generation (an admin POST, say) are in the database by then,
and the incoming generation reads them back into its in-memory
indexes.

### What the health endpoint says

`/healthz` answers with the process that served it, so the two
generations can be told apart during a handover:

```console
$ curl -s http://127.0.0.1:8080/healthz
{"status":"ok","version":"0.1.0","pid":4712}
```

A generation that is draining answers `503` with
`{"status":"draining","reason":"reload in progress","pid":4711}` —
it has stopped accepting new connections but is still finishing the
requests it took. The `pid` is the only thing the endpoint gained;
the status code of a healthy gateway and the shape of its body are
unchanged.

`pangolin-ngx --status` prints both numbers:

```console
$ pangolin-ngx --status
supervisor: pid 4710 (alive)
generation: pid 4712 (ok)
```

### What a failed handover leaves behind

| what went wrong | what the operator sees |
| --- | --- |
| the new configuration does not parse | the new generation exits during startup; the supervisor sees the end of the readiness pipe and reports it |
| the new generation cannot bind a port | the same — the generation never writes the ready byte |
| the QUIC endpoint cannot bind | the same — for a handover generation the failure is fatal |
| the new generation is slow | the ready timeout runs out; the generation is killed |

In every one of those cases the **running generation never stopped
accepting**: the handover is abandoned before the outgoing generation
is told anything. The operator gets one error line in the log, and
the gateway goes on serving as it was.

The one thing a failed handover does **not** repair is a
configuration that is broken in a way the check does not catch. Run
`pangolin-ngx --check` first — it is the same code path the
generation takes, without the ports.

### What a handover costs

**Direct domains see nothing.** The port is held throughout, and the
in-flight requests are finished. A request that arrives during the
handover is answered by the incoming generation, with the same
status code and the same headers.

**Tunnel domains see a bounded window.** The tunnel sessions are
process-local state — a live QUIC connection or WebSocket that
cannot be inherited by another process — so they cannot be handed
over. They are closed in stage B, and the tun reconnects in the
window that follows. A request that arrives in that window fails
with a 502. The window is bounded by stage B (5 seconds) plus the
tun's reconnect backoff (one second to start with).

**The admin session is gone.** The admin's session store is
in-memory (`crates/admin/src/state.rs`), so the operator has to log
in again after every handover.

**The in-memory counters are gone.** The traffic, bot and referer
counters start at zero, and the admin's charts show a gap where the
old generation's numbers used to be. The side files (the JSONL logs)
are not lost — they are flushed before the generation exits.

### Changing the binary

The generation is started with `execve(argv[0], argv, env)` — **not**
`/proc/self/exe`. The operator replaces the binary with an atomic
`mv`, and `/proc/self/exe` keeps pointing at the inode that was
unlinked: exec'ing it would start the old binary again, and the
handover would change nothing. The path in `argv[0]` is where the
new binary lands.

So the operator's routine is:

```console
$ install -m 0755 pangolin-ngx-new /usr/local/bin/pangolin-ngx
$ systemctl reload pangolin-ngx
```

The new binary is picked up by the next generation. The running
generation keeps the old one until it is replaced.

### Upgrading from a version without `SO_REUSEPORT`

A gateway that was started from a binary **without** `SO_REUSEPORT`
cannot hand over: the new generation's bind of the same port fails
with `EADDRINUSE`, the handover fails, and the running generation
keeps serving — which is the safe outcome, but not the one the
operator wanted.

The fix is a **normal restart**: `systemctl restart pangolin-ngx`
starts a generation from the new binary, and the generations after
that can hand over to each other.

The same is true for a gateway that was started before the PID file
existed: the file is written by the supervisor, and the first
supervisor writes it.
