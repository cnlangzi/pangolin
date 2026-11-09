# Knownbots — the verification model

> **Status:** implemented
> **Code:** `crates/knownbots/`
> **Configs:** `crates/knownbots/conf.d/` (embedded), `<log.bot.cache_dir>/conf.d/` (operator)
> **Contract test:** `crates/knownbots/src/bot.rs` →
> `bot::tests::embedded_bot_configs_satisfy_the_contract`

`crates/knownbots` answers exactly one question: **is this request a real
bot?** It is a Rust port of the upstream Go library
[`github.com/cnlangzi/knownbots`](https://github.com/cnlangzi/knownbots).
The port carries the code; this document carries the **contract** — what a bot
config file must contain, what a verified request actually guarantees, and
which parts of upstream are deliberately left out.

The log pipeline that consumes the verdict (JSONL, SSE, per-bot counters) is
documented in [`bot-log.md`](./bot-log.md). The operator-facing knobs are in
the `[log]` table of [`../configuration.md`](../configuration.md#log).

**Glossary**

| Term | Meaning |
| ---- | ------- |
| **Registry** | the set of bot definitions the verifier knows — the embedded configs plus whatever an operator drops into `<log.bot.cache_dir>/conf.d/` |
| **Embedded config** | a YAML file in `crates/knownbots/conf.d/`, listed in `EMBEDDED_CONFIGS` in `crates/knownbots/src/bot.rs` and compiled into the binary with `include_str!` |
| **Operator config** | a YAML file an operator drops into `<log.bot.cache_dir>/conf.d/`; one whose bot `name` matches an embedded bot replaces it |
| **UA marker** | the case-sensitive substring in the `User-Agent` header that identifies a bot (`ua:` in the config) |
| **Prefix set** | the set of CIDR ranges a bot's requests are checked against — the downloaded list of a URL-backed bot, plus the `custom:` entries of the config |
| **PTR record** | the reverse-DNS hostname an IP resolves to |
| **Domain list** | the `domains:` entries of an RDNS bot; a PTR record is accepted only when it matches one of them |

## How a request is verified

A request is a known bot only when **both** of these hold:

1. the **UA marker** of one registry bot matches the request's `User-Agent`
   header — case-sensitively and on a word boundary; **and**
2. the **client IP** belongs to that bot's **address space**.

Either half alone is not enough. A `User-Agent` is a claim anybody can make;
the IP is the proof that the claimant owns the address space it claims. A
request that satisfies only the first half is a forgery and is treated as an
ordinary request.

### The UA marker

`crates/knownbots/src/ua.rs` matches the marker **byte-exactly**:

- **Case-sensitive.** Official crawlers publish a fixed token (`Googlebot`,
  never `googlebot`). Folding the case would accept forgeries that only
  change the casing, so `googlebot` does not match `Googlebot`.
- **Word-boundary anchored.** Alphanumeric characters on either side
  disqualify the match — `MyGooglebot` does not contain the word
  `Googlebot`. A hyphen *is* a boundary, so `Applebot-Extended` contains the
  word `Applebot`.
- **Longest marker wins.** When several markers match one header, the longest
  one wins, so a specific bot shadows a generic one: a
  `Applebot-Extended/1.0` header contains the word `Applebot` *and* the word
  `Applebot-Extended`, and resolves to `applebot-extended`.

The registry id (`name`) of the matched bot — not the raw header — is what
carries forward. The raw `User-Agent` is stored separately in the log row, so
readers never re-scan it.

### The address space — three sources

A bot's address space is assembled from up to three sources. They are
independent: a bot may use one, two, or all three.

| Source | Config | Where it comes from |
| ------ | ------ | ------------------- |
| **Downloaded prefix list** | `urls:` (+ `parser:`) | the CIDRs the vendor publishes, fetched by the background scheduler and cached in `<log.bot.cache_dir>/<bot>/ips.txt` |
| **PTR record** | `rdns: true` + `domains:` | the reverse-DNS hostname of the client IP, accepted when it equals one of the `domains:` entries or is a subdomain of one |
| **`custom:` CIDRs** | `custom:` | static ranges written by hand — a vendor range the crate does not download, or a private range |

The `custom:` CIDRs are merged into the same prefix set as the downloaded
list, so a bot can be half static, half dynamic.

### The order the verifier uses

`Validator::verify` checks the **prefix set first** (an in-memory CIDR
scan), the **PTR path second** (a cache read). A bot that has both is
verified through whichever matches.

The PTR path is the only part that can touch the network, and it never does
so on the request path: a cache miss queues a background lookup and the
request that triggered it fails closed. See
[What is not logged](#what-is-not-logged).

### What the verdict carries

| Verdict field | Where it ends up |
| ------------- | ---------------- |
| `name` (the registry id, e.g. `googlebot`) | JSONL `bot_name`, and the `P-Known-Bot` response header (empty when the request is not a verified bot) |
| `vendor` (e.g. `Google`) | JSONL `bot_vendor` |
| `kind` (e.g. `SearchEngine`) | JSONL `bot_category`, after the coarse bucket mapping (`search_engine`, `ai_bot`, `social`, `ads_bot`, `monitoring`) |

The registry id is the identity of the bot everywhere in the system. It is
*not* the UA marker: `applebot-extended` has the marker `Applebot-Extended`,
and `semrushbot-ba` has the marker `SemrushBot-BA`.

## What is not logged

The verifier **fails closed**: everything that is not positively verified is
an ordinary request. No bot-log row is written, no bot counter moves, and
`P-Known-Bot` stays empty. The generic access log still records it — a
forged bot is still a request.

Four consequences follow, each from a specific mechanism:

- **A forged User-Agent is never logged.** The UA marker matches but the
  client IP is not in the bot's address space, so the verdict is `Failed`.
  Changing the casing of the marker, or sending it from an address the vendor
  does not publish, buys nothing.

- **A URL-backed bot whose list has not been downloaded yet logs nothing.**
  Until the first refresh lands, the prefix set of a URL-backed bot is
  empty, so every claim against it fails closed. The startup log says how
  many bots are in that window, and the *Verification status* panel on
  `/logs/bots` lists them as `Cold` until the scheduler's first successful
  download.

- **The first request from an IP of an RDNS bot is not logged.** The PTR
  lookup is asynchronous: a cache miss queues the lookup and the request
  that triggered it fails closed (`Pending`). Once the cache is warm — the
  lookup succeeded and the hostname matched the bot's `domains:` list — the
  **next** request from the same IP is logged. A lookup that fails with
  "not found" puts the IP into the bot's fail cache, and that IP stays
  unverified.

- **A bot whose list never loaded stays unverified.** A failed refresh
  leaves the bot unverified for as long as it has no prefix set at all — a
  cold bot whose first download failed never verifies, and its traffic never
  reaches the log. A refresh failure never wipes a prefix set that is
  already loaded: a bot that already has addresses keeps verifying from that
  cache, and the failure is reported in the *Verification status* panel
  instead. That distinction is the difference between "no crawlers came" and
  "our IP list never downloaded".

## Bot configuration reference

A bot config is one YAML document. These are all the fields the loader
reads:

| Field | Type | Required | Missing / empty means |
| ----- | ---- | -------- | --------------------- |
| `name` | string | **yes** | the file is skipped (embedded: fatal) — see rule 1 below |
| `ua` | string | **yes** | the file is skipped (embedded: fatal) — see rule 2 below |
| `kind` | enum | no | `Unknown` → the `monitoring` bucket. One of `SearchEngine`, `SocialMedia`, `AITraining`, `AIAssist`, `AIMixed`, `SEO`, `Monitor`, `Security`, `Scraper`; an unrecognised value becomes `Unknown` |
| `vendor` | string | no | falls back to a **name-based heuristic** (`vendor_for`) — `google*` → `Google`, `bingbot` → `Microsoft`, `gptbot` → `OpenAI`, … A name the heuristic does not know becomes `Unknown` |
| `parser` | string | no | **defaults to `txt`** — the list is read as plain CIDR / IP lines |
| `urls` | list of strings | no | the bot is not URL-backed; its address space comes from `custom:` and / or `domains:` |
| `custom` | list of CIDR strings | no | no static address space |
| `domains` | list of strings | no | an RDNS bot with no domain list can never match a PTR record |
| `rdns` | bool | no | `false` — the bot has no PTR path |

Notes:

- **`name` is the id.** It is the value written to the JSONL `bot_name` field
  and to the `P-Known-Bot` header. It is also the key an operator override
  replaces on.
- **`parser` only matters for a URL-backed bot.** The list parsers this
  crate implements are `google`, `openai`, `uptimerobot`, `ahrefs`, `amazon`
  and `txt`. An unknown parser is *not* an error at load time — the list is
  then parsed as plain text, which usually yields no prefixes at all.
- **A `domains:` list is inert without `rdns: true`.** The list is only
  consulted on the PTR path.
- **Fields this crate does not read are ignored.** A config carrying a key
  the struct does not know (`asn:` from upstream, say) parses fine and the
  key has no effect.

### The contract

A bot config file must satisfy these rules. They are enforced by
`bot::tests::embedded_bot_configs_satisfy_the_contract`, which runs offline
over the embedded set:

1. **`name` is non-empty and unique across the registry.** The loader keeps
   the *last* bot of a duplicated name, so without this rule one bot
   silently replaces the other.
2. **`ua` is non-empty.** An empty marker matches no `User-Agent`, so the bot
   would never verify.
3. **If `urls` is non-empty, `parser` is absent (which means `txt`) or one
   of the parsers the crate implements** — `google`, `openai`, `uptimerobot`,
   `ahrefs`, `amazon`, `txt`. The crate-internal `parser::is_known` is the
   single source of that list.
4. **If `rdns` is `true`, `domains` contains at least one entry.** An RDNS
   bot without a domain list can never match a PTR record: it is
   permanently unverified and its traffic never reaches the bot log.
5. **Every `custom` entry parses as a CIDR.** The loader skips a bad one
   with a warning, so the bot would silently cover less address space than
   the config claims.
6. **Every `*.yaml` / `*.yml` file in `crates/knownbots/conf.d/` is listed
   in `EMBEDDED_CONFIGS`.** A file that is not compiled into the binary is
   never parsed, so it does nothing at all.

Rules 3, 4 and 5 are exactly the failures that are invisible today: the
loader only warns, and the bot then never verifies a request — a failure
nothing reports. Rule 6 catches the inverse mistake: a config written into
`conf.d/` but never added to `EMBEDDED_CONFIGS`.

```bash
# The contract test — offline, no network access.
cargo test -p knownbots --lib embedded_bot_configs_satisfy_the_contract

# The real-address test — needs network access, checks that every
# embedded bot still verifies against a published address.
cargo test -p knownbots --test known_ips
```

## Adding or changing a bot

### An embedded bot

1. Write the YAML file into `crates/knownbots/conf.d/`.
2. List it in `EMBEDDED_CONFIGS` in `crates/knownbots/src/bot.rs`:

   ```rust
   ("yourbot.yaml", include_str!("../conf.d/yourbot.yaml")),
   ```

   The **file name is the key** in that list. It does **not** have to equal
   the `name` field — `semrushbot-backlinks.yaml` declares
   `name: semrushbot-ba`. The file name is only the lookup key for
   `include_str!`; the `name` field is the registry id.
3. Check the contract:

   ```bash
   cargo test -p knownbots --lib
   ```

   The contract test fails with **every** violation in one message, not just
   the first.
4. Give the bot a `vendor:`. Without one the JSONL `bot_vendor` falls back
   to a name-based heuristic, and to `Unknown` for a name the heuristic does
   not know.

Removing a bot is the same in reverse: delete the file **and** its
`EMBEDDED_CONFIGS` entry. A file left behind without the entry is caught by
rule 6.

### An operator override

An operator override is a YAML file dropped into
`<log.bot.cache_dir>/conf.d/` (default `./logs/bots/cache/conf.d/`). It is
loaded after the embedded configs, and a file whose bot `name` matches an
embedded bot **replaces** that bot — same fields, same schema.

```yaml
# ./logs/bots/cache/conf.d/mybot.yaml
kind: SEO
name: mybot
vendor: "Example"
ua: "MyBot"
parser: txt
urls:
  - "https://example.com/mybot-ips.txt"
```

An operator file that cannot be turned into a bot — a YAML syntax error, or
a document without `name` / `ua` — is **skipped and reported**, not fatal: a
typo in an override file must not take the gateway down. The file path and
the reason show up on `/logs/bots`. The contract test does **not** cover
operator files — it runs over the embedded set only, because that is the set
this repository controls.

## Relationship with upstream cnlangzi/knownbots

This port tracks upstream, but not one-for-one. Upstream ships **57** bot
configs; this port embeds **45**.

### The 23 upstream configs this port does not embed

| Group | Count | What they are | Why they are absent |
| ----- | ----- | ------------- | ------------------- |
| HTTP-client / tooling markers | 20 | 19 of them carry **no address space at all** — only a `ua:` marker (`axios`, `wget`, `python-requests`, `okhttp`, `curl`, `node-fetch`, `rust-reqwest`, …). The twentieth, `github`, is backed upstream by GitHub's own webhook / CI ranges | the 19 identify a *client tool* for upstream's opt-in User-Agent **classification** — they carry no address space at all, so they can never verify a request, and importing them would add registry entries that could never be verified. `github` is a real bot upstream, but verifying it here would need a `github` list parser, which this crate does not implement |
| `google-special` | 1 | upstream's combined AdsBot / FeedFetcher / MediaPartners entry (`ua: "AdsBot"`, the `special-crawlers.json` list) | **no coverage is lost**: this port covers the same UA marker and the same IP list under the registry id `adsbot`. Upstream's separate `feedfetcher` and `mediapartners-google` entries have their counterparts here as well |
| Genuine crawlers | 2 | `chrome-lighthouse` (a reverse-DNS bot) and `stripe` (a URL-backed bot that would also need a `stripe` list parser) | **absent, not forgotten**. Adding `stripe` needs a new list parser in `crates/knownbots/src/parser.rs`; `chrome-lighthouse` is a single audit-tool crawler. Both are out of scope for a documentation change |

### The 11 bots this port ships that upstream does not have

`applebot-extended`, `bytespider`, `ccbot`, `claudebot`, `discordbot`,
`perplexitybot`, `pingdom`, `redditbot`, `slackbot`, `telegrambot`,
`whatsapp`.

### The ASN step is not ported

Upstream verifies in the order **IP ranges → ASN → RDNS**; this port
implements **IP ranges → RDNS**. Upstream's ASN step (an `asn:` list per bot,
backed by its own route-collector fetches) is not ported: **no upstream
config uses the `asn:` field**, so no bundled bot is affected. A config that
carries one parses here and the key is ignored.

### Deliberate behavioural differences

- **An absent `parser:` means `txt` here.** Upstream substitutes the bot's
  *name* for an empty parser, which then falls through to the plain-text
  parser anyway — the same result by a longer road, and a confusing one to
  debug.
- **The PTR lookup is asynchronous here.** Upstream resolves the hostname
  inline on the request path; this port queues the lookup and fails the
  current request closed. See [What is not logged](#what-is-not-logged).
- **`vendor:` is a pangolin addition.** Upstream's configs have no vendor
  field at all; the JSONL `bot_vendor` and the admin UI's vendor column are
  this port's own.
- The **34 configs both projects ship** are otherwise identical, with one
  exception: `google-inspectiontool` is `kind: SearchEngine` here and
  `kind: Monitor` upstream.

There is **no automated upstream sync**. CI has no guaranteed network
access, and the comparison is cross-language — the config sets are compared
by hand, and this document is the record of that comparison.
