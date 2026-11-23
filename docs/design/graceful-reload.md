# Graceful reload — the groundwork

Replacing the gateway's binary or its configuration without a
downtime window needs three things, and this document describes the
three the gateway has today:

1. **two processes can hold the same port at the same time** —
   `SO_REUSEPORT` on every listener (issue #233),
2. **the operator can tell whether the new configuration is usable
   before the switch** — `pangolin-ngx --check` (issue #233),
3. **the operator can address the running process by its number** —
   the PID file (issue #233).

What is **not** here yet: the handover itself. The signal that
triggers it, the supervisor that starts the second generation, the
readiness pipe and the drain of the first generation are a later
issue. This document describes the infrastructure they will rest on,
so that the three pieces can be used on their own in the meantime —
a blue/green pair of instances is possible today, and so is "check
the configuration before I restart".

For the configuration keys, see
[`configuration.md`](../configuration.md) → `[pid]`.

## Contents

- [`SO_REUSEPORT` on every listener](#so_reuseport-on-every-listener)
- [`pangolin-ngx --check`](#pangolin-ngx---check)
- [The PID file](#the-pid-file)

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

- The gateway writes the file **after** it has read its configuration
  and **before** it binds any listener. The file says "this process
  intends to serve", not "the ports are up".
- A graceful exit removes it.
- A `SIGKILL` leaves it behind. The next start truncates it, so a
  stale number never survives a restart.

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

The three pieces above are the groundwork. The handover itself —
the signal that triggers it, the process model that starts the second
generation, the readiness handshake and the drain of the first one —
is a separate issue.

Two things are worth saying about it here:

- the PID file is written **before** the listeners are bound, so it
  cannot be used as a readiness signal. A handover that waits for the
  new generation's PID file would start the traffic too early.
- the QUIC endpoint degrades **silently** when it cannot bind. A
  handover that does not check the QUIC listener's log line would
  push every tunnel onto the WebSocket/TCP fallback without saying
  so.
