# The admin API (`/api/*`)

> **Conventions**
>
> 1. These endpoints live on the **admin listener** (`admin.addr`) — not on the dash API's channel and not on the proxy's port.
> 2. A mutating endpoint needs the session's `_csrf` token: in the **request body** for `POST` / `PUT` / `PATCH`, in the **query string** for `DELETE`.
> 3. The authentication and the login flow are the admin UI's own and are **out of scope** for this document.

The admin UI has an HTTP surface of its own: the `/api/*` endpoints its
browser talks to. It is **not** a published contract — it changes with
the admin UI, and a dashboard must not depend on it. The contract the
dashboard implements against is [dash.md](dash.md).

The tables below list every `/api/*` endpoint: the method, the path,
the type of the answer and what it is for. The **fields** of an answer
belong to the template or the handler that renders it — they change
with the UI, so they are not repeated here.

## HTMX fragments

The admin UI swaps these into the page in place; they are HTML
fragments, not JSON.

| method | path | answer | what for |
| --- | --- | --- | --- |
| `GET` | `/api/site/{name}/domains` | `text/html` fragment | the domain table of one site, for the site's sub-page |
| `GET` | `/api/site/{name}/domains/new` | `text/html` fragment | the new-domain form, with the site preselected |
| `GET` | `/api/traffic/kpis` | `text/html` fragment | the KPI strip of the traffic panel |
| `GET` | `/api/traffic/tables` | `text/html` fragment | the per-site traffic tables |
| `GET` | `/api/ratelimit/kpis` | `text/html` fragment | the KPI strip of the rate-limit panel |
| `GET` | `/api/ratelimit/sites` | `text/html` fragment | the per-site rate-limit table and the top over-limit clients |
| `GET` | `/api/ratelimit/rules` | `text/html` fragment | the IP-rule table; it carries the substituted `_csrf` the delete buttons need |
| `GET` | `/api/ratelimit/blocked` | `text/html` fragment | the live list of blocked clients |
| `GET` | `/api/cache/card` | `text/html` fragment | the polled edge-cache card of the dashboard |
| `GET` | `/api/logs/bots/summary` | `text/html` fragment | the polled summary region of the crawler-log page |
| `GET` | `/api/logs/referers/summary` | `text/html` fragment | the polled summary region of the referer-log page |

## Mutations

Every one of these needs the session's `_csrf` token. The `DELETE`s
answer `200` with an **empty** `text/html` body; the `POST` answers a
redirect.

| method | path | answer | what for |
| --- | --- | --- | --- |
| `DELETE` | `/api/domains/{domain}` | `200`, `text/html`, empty body | delete the domain and its certificate row |
| `DELETE` | `/api/sites/{name}` | `200`, `text/html`, empty body | delete the site |
| `DELETE` | `/api/certs/{domain}` | `200`, `text/html`, empty body | delete the certificate of a domain and relink the rest |
| `DELETE` | `/api/tun/{name}` | `200`, `text/html`, empty body | delete the tunnel node |
| `DELETE` | `/api/dns/{name}` | `200`, `text/html`, empty body | delete the DNS provider and clear the domains that referenced it |
| `DELETE` | `/api/ratelimit/rules` | `200`, `text/html`, empty body | delete one IP rule; `scope`, `kind` and `cidr` are in the query string |
| `POST` | `/api/domains/{domain}/edit` | `302` to `/domains` | update a domain — the edit form posts here |

## JSON

| method | path | answer | what for |
| --- | --- | --- | --- |
| `GET` | `/api/certs/summary` | `application/json` | the certificate counters of the dashboard |
| `POST` | `/api/reload` | `application/json` | reload the in-memory indexes from the database — see [Reload API](../admin/reload-api.md) for the use and the caveats |
| `GET` | `/api/system/config` | `application/json` | the system configuration singleton |
| `POST` | `/api/system/config` | `302` to `/` | update the system configuration singleton; the body is form-encoded |
| `GET` | `/api/badbots/config` | `application/json` | the bot-detection settings |

## Server-Sent Events

The three live feeds of the log pages. Each is a chunked
`text/event-stream`: the body replays the ring buffer first and then
carries every entry that arrives.

| method | path | answer | what for |
| --- | --- | --- | --- |
| `GET` | `/api/logs/stream` | `text/event-stream` | the access log |
| `GET` | `/api/logs/bots/stream` | `text/event-stream` | the crawler log |
| `GET` | `/api/logs/referers/stream` | `text/event-stream` | the referer log |

## What is not here

- The admin UI's **HTML pages** (`/dashboard`, `/sites`, `/settings`, …)
  and the **static assets** (`/assets/*`).
- The **login flow** and the session cookie.
- The **health checks** (`/health`, `/ping`, `/healthz`).
- The **dash API** (`/api/v1/`) — the channel the dashboard speaks,
  documented in [dash.md](dash.md).
