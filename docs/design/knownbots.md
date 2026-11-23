# Knownbots — the verification model

> **Status:** implemented
> **Code:** the published `knownbots` crate —
> [`github.com/cnlangzi/knownbots-rust`](https://github.com/cnlangzi/knownbots-rust),
> consumed from crates.io as `knownbots` 0.4.0 (issues #146, #157, #230)
> **Configs:** the crate's own `conf.d/` (embedded),
> `<log.bot.cache_dir>/conf.d/` (operator)
> **Contract test:** the crate's
> `model::registry::tests::embedded_bot_configs_satisfy_the_contract`

The `knownbots` crate answers exactly one question: **is this request a real
bot?** It is a Rust port of the upstream Go library
[`github.com/cnlangzi/knownbots`](https://github.com/cnlangzi/knownbots).
The port carries the code; this document carries the **contract** — what a bot
config file must contain, what a verified request actually guarantees, and
which parts of upstream are deliberately left out.

The crate lives in **its own repository** and is **published to crates.io**
(`knownbots` 0.4.0); pangolin consumes it from the registry and no longer
carries a copy. What stays here is the consumer-side contract: the admin
panel's view of the verification layer and the operator-facing knobs. The
category vocabulary and the mapping into it are the crate's — see
[The category vocabulary](#the-category-vabulary).

The log pipeline that consumes the verdict (JSONL, SSE, per-bot counters) is
documented in [`bot-log.md`](./bot-log.md). The operator-facing knobs are in
the `[log]` table of [`../configuration.md`](../configuration.md#log).

**Glossary**

| Term | Meaning |
| ---- | ------- |
| **Registry** | the set of bot definitions the verifier knows — the embedded configs plus whatever an operator drops into `<log.bot.cache_dir>/conf.d/` |
| **Embedded config** | a YAML file in the crate's `conf.d/`, listed in `EMBEDDED_CONFIGS` in its `src/model/registry.rs` and compiled into the crate with `include_str!` |
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

The crate's UA matcher (`src/index/ua.rs`) matches the marker **byte-exactly**:

- **Case-sensitive.** Official crawlers publish a fixed token (`Googlebot`,
  never `googlebot`). Folding the case would accept forgeries that only
  change the casing, so `googlebot` does not match `Googlebot`.
- **Word-boundary anchored.** Alphanumeric characters on either side
  disqualify the match — `MyGooglebot` does not contain the word
  `Googlebot`. A hyphen *is* a boundary, so `Applebot-Extended` contains the
  word `Applebot`.
- **Longest marker wins.** When several markers match one header, the longest
  one wins, so a specific bot shadows a generic one: a
  `SemrushBot-SI/1.0` header contains the word `SemrushBot` *and* the word
  `SemrushBot-SI`, and resolves to `semrushbot-si`.

The registry id (`name`) of the matched bot — not the raw header — is what
carries forward. The raw `User-Agent` is stored separately in the log row, so
readers never re-scan it.

### The 0.2 registry change

The registry is **data inside the published crate**, not code in this
repository — so the gateway needs no change to consume a new registry
version, but the change is visible in four places.

**The markers are the tokens the vendors publish.** The marker of a registry
entry is the exact string the vendor puts in its own `User-Agent` header,
and the match is case-sensitive, so a marker that is spelled differently can
never fire. knownbots 0.2 corrected six of them:

| Registry id | Marker before 0.2 | Marker in 0.2 |
| ----------- | ----------------- | ------------- |
| `twitterbot` | `twitterbot` | `Twitterbot` |
| `pinterestbot` | `PinterestBot` | `Pinterestbot` |
| `redditbot` | `Redditbot` | `redditbot` |
| `mj12bot` | `MJ12Bot` | `MJ12bot` |
| `meta-externalagent` | `Meta-ExternalAgent` | `meta-externalagent` |
| `google-storebot` | `GoogleStoreBot` | `Storebot-Google` |

Those crawlers were **invisible** to the gateway before 0.2 — a request
carrying `Twitterbot/1.0` matched nothing, so it was not a verified bot and
was not exempt from the rate limits. They are visible now.

**Three crawlers are gone.** `applebot-extended`, `google-extended` and
`cloudflare-alwaysonline` are no longer in the registry: **a token the
crawler does not send is no User-Agent at all**. `Google-Extended` and
`Applebot-Extended` are opt-out tokens a vendor writes in `robots.txt`, not
User-Agents, and Cloudflare publishes no `AlwaysOnline` crawler — a
definition whose marker no request carries would sit in the registry
advertising a traffic it can never recognise. A request carrying
`Applebot-Extended` still matches the marker `Applebot` (a hyphen is a word
boundary) and is now identified as `applebot`; a request carrying
`Google-Extended` matches nothing and is no longer a verified bot.

**Two crawlers changed their verification source.** `bytespider` and
`chatgpt-user` now verify from the vendor's published address list instead
of their reverse DNS, so they **fail closed** until the first list refresh
has succeeded.

The per-site bot list (`site_rate_limit_bots`) stores registry ids, so the
three removed ids leave the table inert. The V14 migration rewrites
`applebot-extended` to `applebot` — the request now identifies as
`applebot`, and a site that deliberately kept it out of its exemptions must
keep doing so — and deletes the two ids with no successor.

### The 0.4 category change

Since 0.4.0 the crate owns the **category** of a crawler, and every bot
definition states its own. Two things follow for the operator.

**A bot config that names no `category` is skipped.** The loader reads
`category` as a required field: a document without one is incomplete, so
it never reaches the registry. The crawler in it is then **not verified** —
its traffic is not recognised as a bot's, it gets no bot-log row, no
`P-Known-Bot` header and no rate-limit exemption, and it is missing from
the site's bot list. The file is **not** fatal: the rest of the registry
loads, the gateway starts, and the file is reported as skipped — on
`/logs/bots` under "Verification status", and in the gateway log at
startup:

```
WARN bot config skipped: /var/lib/pangolin/logs/bots/cache/conf.d/mybot.yaml — missing required 'category' field
```

**So an existing override file needs one line.** A file written for 0.3.0
carries `name` / `ua` / `vendor` / `kind` and no `category`:

```yaml
# before (knownbots 0.3.0) — skipped by 0.4.0
kind: SEO
name: mybot
vendor: "Example"
ua: "MyBot"
```

```yaml
# after (knownbots 0.4.0 and later)
kind: SEO
category: search_engine
name: mybot
vendor: "Example"
ua: "MyBot"
```

The category is **not** derived from the `kind` — the kind says what the
crawler does on the web, the category says which dashboard it belongs to.
The bucket that fits a `Monitor` is a choice: `monitoring` or `ads_bot`
(see [The category vocabulary](#the-category-vocabulary)).

Nothing else changes for the operator: the embedded bots of the crate
already carry their category, and the wire labels of the rows are the
same as before.

### The address space — three sources

A bot's address space is assembled from up to three sources. They are
independent: a bot may use one, two, or all three.

| Source | Config | Where it comes from |
| ------ | ------ | ------------------- |
| **Downloaded prefix list** | `urls:` (+ `parser:`) | the CIDRs the vendor publishes, fetched by the background scheduler and cached in `<log.bot.cache_dir>/<bot>/ips.txt` |
| **PTR record** | `rdns: true` + `domains:` | the reverse-DNS hostname of the client IP, accepted when it equals one of the `domains:` entries or is a subdomain of one (both sides are lower-cased and stripped of their trailing dots) |
| **`custom:` CIDRs** | `custom:` | static ranges written by hand — a vendor range the crate does not download, or a private range |

The `custom:` CIDRs are merged into the same prefix set as the downloaded
list, so a bot can be half static, half dynamic.

### Downloading a published list

The crate downloads the list itself, with the clients it builds
(`knownbots::http::build_clients()`). **Since 0.3.0 a download that the
endpoint refuses is repeated over the other address family** (issue #157),
and the family that delivered the list is remembered for the host.

The crate builds **three** clients: one that reaches every address of a
host, one bound to the IPv4 addresses only, and one bound to the IPv6
addresses only. A document takes at most three attempts:

1. the client of the family that delivered the list of that host the last
   time — or, for a host seen for the first time, the unbound client;
2. the client of the other family, when the first attempt was **refused** (a
   status of the 2xx range other than 200, or a 200 whose body the parser
   does not read) or when the route to the vendor failed;
3. the client of the family that is left.

The family that delivered the document is **remembered for the host**, so
the next list of the same vendor goes straight to the client that works.

The reason is measured, not hypothetical: on a host whose IPv6 path to Bing
is tampered with, `https://www.bing.com/toolbox/bingbot.json` answers
`203 Non-Authoritative Information` with a two-byte body over IPv6, and
`200 application/json` with the 29 published prefixes over IPv4. The
refusal over IPv6 is repaired by the repetition over IPv4, and the family
that answered is remembered. Before, the answer was read as a JSON syntax
error — a message that reads as "our code is at fault" — and `bingbot` kept
an empty prefix set: every request claiming to be that crawler failed
closed, never reached the bot log, and was throttled like an ordinary
client.

The gateway therefore **does not supply a `client_set` of its own**
(`ValidatorOptions::client_set` stays `None`): a host-supplied set of
clients keeps its own choice of the address family, and the bookkeeping of
the families lives in the transport the crate builds.

**A refused download names what the endpoint answered.** The message shapes:

| What the endpoint did | The message | The panel shows |
| --------------------- | ----------- | -------------- |
| A status other than 200 | `{url} returned {status} ({content_type}): {excerpt}` | `unreachable` |
| A status other than 200, no body | `{url} returned {status}` | `unreachable` |
| 200, a body the parser cannot read | `{url} answered 200 with {content_type}: {excerpt}` | `unreachable` |
| 200, a document whose shape changed | `{url}: {reason}` | `broken` |

A status the upstream decided itself — `401`, `403`, `404`, `429`, `5xx` — is
**never** a refusal: it is the vendor's answer about the request, so it is
not repeated over the other family. A `429` or a `5xx` is transient and is
retried twice (3 s apart, or the `Retry-After` of the answer) on the same
family; every other status fails at once.

The **excerpt** is the first at most 64 bytes of the body, decoded as UTF-8
with a lossy conversion, with the control characters replaced by `·` and a
`…` appended when the body is longer. So the tampered path above reads:

```
https://www.bing.com/toolbox/bingbot.json returned 203 Non-Authoritative Information (text/xml): OK
```

The **URL in a message is sanitized** — its query and its fragment are cut
off, so a token of the vendor does not reach the log.

A list that fails to refresh **keeps the prefixes that were already loaded**
— the crawler keeps verifying from them.

The crate logs through `tracing`, and pangolin installs no tracing
subscriber, so the reason reaches the operator through
`Validator::refresh_status()` and the "Verification status" panel, not
through the gateway log. The one line the operator misses is the repair
itself — *"the endpoint refused the request; the other address family
delivered the list"* — which the crate logs and pangolin drops.

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
| `kind` (e.g. `SearchEngine`) | the fine-grained kind of the crawler — **not persisted**; the row carries the coarse `category` |
| `category` (e.g. `SearchEngine`) | JSONL `bot_category`, the coarse bucket of the crate's vocabulary (`search_engine`, `ai_bot`, `social`, `ads_bot`, `monitoring`) |

The registry id is the identity of the bot everywhere in the system. It is
*not* the UA marker: `semrushbot-si` has the marker `SemrushBot-SI`, and
`adsbot-mobile` has the marker `AdsBot-Google-Mobile`.

### The category vocabulary — the crate's

The crate classifies a bot by `BotKind` (`SearchEngine`, `SocialMedia`,
`AITraining`, `AIAssist`, `AIMixed`, `SEO`, `Monitor`, `Security`, `Scraper`,
`Unknown`) — that is the fine-grained kind, what the crawler does on the web.
The **category** is the coarse bucket the admin UI groups by, and since 0.4.0
the crate owns it: every bot definition states its own `category:`, the crate
publishes the `BotCategory` type with its wire labels, and the host consumes
both. Pangolin keeps no second copy of the enum, so there is no second source
of truth that could drift away from the labels the JSONL rows carry.

| `BotKind` | Category | Wire label |
| --------- | -------- | ---------- |
| `SearchEngine`, `SEO` | `SearchEngine` | `search_engine` |
| `AITraining`, `AIAssist`, `AIMixed` | `AiBot` | `ai_bot` |
| `SocialMedia` | `Social` | `social` |
| `Monitor` — the ad-quality crawlers | `AdsBot` | `ads_bot` |
| `Monitor` — every other one, `Security`, `Scraper`, `Unknown` | `Monitoring` | `monitoring` |

A `Monitor` bot states which of the two buckets it is in: the ad-quality
crawlers (`adsbot`, `adsbot-mobile`, `mediapartners-google`, `oai-adsbot`)
say `ads_bot`, every other monitor says `monitoring`. The relation between a
kind and a category lives in the crate's registry, and the crate's own
contract test checks that the category a definition states is one its kind
allows.

The wire labels are the JSONL `bot_category` of every row and the values of
the `/logs/bots` filters; they must not drift. They are the crate's
vocabulary now — the label the crate publishes is the label the rows carry.

The **badge** that paints a category is pangolin's own vocabulary and stays
here: `BotCategoryStyle` in `crates/pangolin-core/src/bot.rs` is an
extension trait — the category type belongs to the crate, so it cannot carry
an inherent method — that maps a category to its Tailwind classes. Its
fallback branch paints a bucket the crate gains later the neutral way, the
same class the live-tail page's `categoryClass()` uses for a label it does
not know.

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
| `category` | enum | **yes** | the file is skipped (embedded: fatal) — see rule 7 below. One of `search_engine`, `ai_bot`, `social`, `monitoring`, `ads_bot`; a value the crate does not know is skipped the same way |
| `kind` | enum | no | `Unknown`. One of `SearchEngine`, `SocialMedia`, `AITraining`, `AIAssist`, `AIMixed`, `SEO`, `Monitor`, `Security`, `Scraper`; an unrecognised value becomes `Unknown` |
| `vendor` | string | no | falls back to a **name-based heuristic** (`vendor_for`) — `google*` → `Google`, `bingbot` → `Microsoft`, `gptbot` → `OpenAI`, … A name the heuristic does not know becomes `Unknown` |
| `parser` | string | no | **defaults to `txt`** — the list is read as plain CIDR / IP lines |
| `urls` | list of strings | no | the bot is not URL-backed; its address space comes from `custom:` and / or `domains:` |
| `custom` | list of CIDR strings | no | no static address space |
| `domains` | list of strings | no | an RDNS bot with no usable domain list can never match a PTR record |
| `rdns` | bool | no | `false` — the bot has no PTR path |

Notes:

- **`name` is the id.** It is the value written to the JSONL `bot_name` field
  and to the `P-Known-Bot` header. It is also the key an operator override
  replaces on.
- **`category` is the coarse bucket the crawler is grouped into**, and it is
  the crate's vocabulary: the definition states it, the crate publishes the
  labels, and the JSONL `bot_category` of a row is the label of the
  definition. It is **not** derived from `kind` — the kind says what the
  crawler does on the web, the category says which dashboard it belongs to,
  and the two are facts of their own. A `Monitor` states `monitoring` or
  `ads_bot`; the crate's contract test checks that the category a definition
  states is one its kind allows.
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
`model::registry::tests::embedded_bot_configs_satisfy_the_contract`, which runs offline
over the embedded set:

1. **`name` is non-empty and unique across the registry.** The loader keeps
   the *last* bot of a duplicated name, so without this rule one bot
   silently replaces the other.
2. **`ua` is non-empty.** An empty marker matches no `User-Agent`, so the bot
   would never verify.
3. **If `urls` is non-empty, `parser` is absent (which means `txt`) or one
   of the parsers the crate implements** — `google`, `openai`, `uptimerobot`,
   `ahrefs`, `amazon`, `txt`. The crate-internal `feed::is_known_parser` is the
   single source of that list.
4. **If `rdns` is `true`, `domains` contains at least one entry that can
   still match a PTR record.** An RDNS bot without a domain list can never
   match a PTR record: it is permanently unverified and its traffic never
   reaches the bot log. An entry that is empty, only dots (`"."`) or
   carrying whitespace is as good as no entry at all — the comparison
   strips the trailing dots and compares the rest literally, so such an
   entry matches nothing.
5. **Every `custom` entry parses as a CIDR.** The loader skips a bad one
   with a warning, so the bot would silently cover less address space than
   the config claims.
6. **Every `*.yaml` / `*.yml` file in the crate's `conf.d/` is listed
   in `EMBEDDED_CONFIGS`.** A file that is not compiled into the crate is
   never parsed, so it does nothing at all.
7. **Every definition states a `category`, and it is one the bot's `kind`
   allows.** A definition that names no category is incomplete: the loader
   skips it, so the crawler never reaches the registry and its traffic is
   never recognised as a bot's. A category the kind does not allow puts the
   crawler in a bucket that does not describe it.

Which of these failures are **silent** — the loader accepts the config and
the bot simply never verifies anything:

| Rule | Failure | Loud or silent |
| ---- | ------- | -------------- |
| 1 | empty `name` | **loud** — `load_embedded` treats an incomplete embedded config as fatal and the gateway refuses to start |
| 1 | duplicate `name` | silent — the loader keeps the *last* bot of that name, so one bot replaces the other |
| 2 | empty `ua` | **loud** — same fatal path |
| 3 | unknown parser | silent — the loader only warns; the list is then parsed as plain text and the refresh ends as "broken" |
| 4 | no usable domain | silent — the bot is permanently unverified |
| 5 | invalid `custom:` CIDR | silent — the loader skips it with a warning |
| 6 | file not embedded | silent — the file is never read at all |
| 7 | no `category` | **loud** — same fatal path as rules 1 and 2 |
| 7 | a category the kind does not allow | silent — the loader does not check the relation; the bot lands in the bucket it named |

The silent rows are the ones this contract test exists for: rules 3, 4, 5
and 6 are exactly the failures that are invisible today, and so is the
duplicate half of rule 1 and the second half of rule 7.

```bash
# The contract test — offline, no network access. It runs in the
# crate's own repository:
cargo test -p knownbots --lib embedded_bot_configs_satisfy_the_contract

# The live test — needs network access, checks that the lists of the
# vendors are still readable. It is `#[ignore]`d by default:
cargo test --all-features --test live -- --ignored
```

## Adding or changing a bot

### An embedded bot

Embedded bots are added in the **crate's own repository**
([`cnlangzi/knownbots-rust`](https://github.com/cnlangzi/knownbots-rust)),
not here — pangolin consumes the published crate and does not carry a copy of
its registry.

1. Write the YAML file into the crate's `conf.d/`.
2. List it in `EMBEDDED_CONFIGS` in its `src/model/registry.rs`:

   ```rust
   ("yourbot.yaml", include_str!("../../conf.d/yourbot.yaml")),
   ```

   The **file name is the key** in that list. It does **not** have to equal
   the `name` field — `semrushbot-backlinks.yaml` declares
   `name: semrushbot-ba`. The file name is only the lookup key for
   `include_str!`; the `name` field is the registry id.
3. State the bot's `category:` — one of the five labels of the crate's
   vocabulary. The category is not derived from the `kind`: an embedded
   definition without one is incomplete, and the loader refuses to start the
   host over it.
4. Check the contract:

   ```bash
   cargo test -p knownbots --lib
   ```

   The contract test fails with **every** violation in one message, not just
   the first.
5. Give the bot a `vendor:`. Without one the JSONL `bot_vendor` falls back
   to a name-based heuristic, and to `Unknown` for a name the heuristic does
   not know.

Removing a bot is the same in reverse: delete the file **and** its
`EMBEDDED_CONFIGS` entry. A file left behind without the entry is caught by
rule 6. A new bot reaches pangolin when the crate is re-published and the
dependency is bumped.

### An operator override

An operator override is a YAML file dropped into
`<log.bot.cache_dir>/conf.d/` (default `./logs/bots/cache/conf.d/`). It is
loaded after the embedded configs, and a file whose bot `name` matches an
embedded bot **replaces** that bot — same fields, same schema.

```yaml
# ./logs/bots/cache/conf.d/mybot.yaml
kind: SEO
category: search_engine
name: mybot
vendor: "Example"
ua: "MyBot"
parser: txt
urls:
  - "https://example.com/mybot-ips.txt"
```

An operator file that cannot be turned into a bot — a YAML syntax error, or
a document without `name` / `ua` / `category` — is **skipped and reported**,
not fatal: a typo in an override file must not take the gateway down. The
file path and the reason show up on `/logs/bots`, and the gateway logs each
skipped file at startup (a crawler that is not in the registry is not
verified, and that is worth a line in the log). The contract test does
**not** cover operator files — it runs over the embedded set only, because
that is the set this repository controls.

## Relationship with upstream cnlangzi/knownbots

This port tracks upstream, but not one-for-one. This port embeds the
crate's own set of bots — **the inventory belongs to the crate**, whose
contract test (`embedded_bot_configs_satisfy_the_contract`) checks it, so
this document does not restate how many bots it holds.

What follows is the comparison with the Go upstream the crate was ported
from. Upstream ships **57** bot configs, and this port deliberately leaves
some of them out:

### The upstream configs this port does not embed

| Group | Count | What they are | Why they are absent |
| ----- | ----- | ------------- | ------------------- |
| HTTP-client / tooling markers | 20 | 19 of them carry **no address space at all** — only a `ua:` marker (`axios`, `wget`, `python-requests`, `okhttp`, `curl`, `node-fetch`, `rust-reqwest`, …). The twentieth, `github`, is backed upstream by GitHub's own webhook / CI ranges | the 19 identify a *client tool* for upstream's opt-in User-Agent **classification** — they carry no address space at all, so they can never verify a request, and importing them would add registry entries that could never be verified. `github` is a real bot upstream, but verifying it here would need a `github` list parser, which this crate does not implement |
| `google-special` | 1 | upstream's combined AdsBot / FeedFetcher / MediaPartners entry (`ua: "AdsBot"`, the `special-crawlers.json` list) | **no coverage is lost**: this port covers the same UA marker and the same IP list under the registry id `adsbot`. Upstream's separate `feedfetcher` and `mediapartners-google` entries have their counterparts here as well |
| Genuine crawlers | 2 | `chrome-lighthouse` (a reverse-DNS bot) and `stripe` (a URL-backed bot that would also need a `stripe` list parser) | **absent, not forgotten**. Adding `stripe` needs a new list parser in the crate's `src/feed/parsers.rs`; `chrome-lighthouse` is a single audit-tool crawler. Both are out of scope for a documentation change |
| Crawlers dropped in knownbots 0.2 | 2 | `google-extended` (upstream's `Google-Extended`) and `cloudflare-alwaysonline` (upstream's `CloudFlare-AlwaysOnline`) | **no coverage is lost, because no request ever carried them**: neither token is a User-Agent a vendor publishes. The port shipped both until 0.2 and dropped them for the same reason it corrected the other markers — see [The 0.2 registry change](#the-02-registry-change) |

### The bots this port ships that upstream does not have

The crate's `conf.d/` is the source of truth for that set, and the crate's
own contract test checks it — so this document does not enumerate it
either.

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
- The configs **both projects ship** are otherwise identical, with these
  exceptions:
  - `google-inspectiontool` is `kind: SearchEngine` here and `kind: Monitor`
    upstream;
  - five markers are spelled the way the **vendor** publishes them and not
    the way upstream spells them — `Twitterbot` (not `twitterbot`),
    `Pinterestbot` (not `PinterestBot`), `MJ12bot` (not `MJ12Bot`),
    `meta-externalagent` (not `Meta-ExternalAgent`) and `Storebot-Google`
    (not `GoogleStoreBot`);
  - `chatgpt-user` and `perplexity-user` are backed by the vendor's
    **published address list** here and by their reverse DNS upstream.

There is **no automated upstream sync**. CI has no guaranteed network
access, and the comparison is cross-language — the config sets are compared
by hand, and this document is the record of that comparison.
