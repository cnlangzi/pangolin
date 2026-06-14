# Admin UI template guide

Conventions for `crates/admin/templates/`. Read this before adding a new
table row, button, or form — most patterns are already in place and
copying them keeps the UI consistent.

## Layout

Templates live under `crates/admin/templates/` and use [askama 0.12](https://docs.rs/askama/0.12):

- `layouts/base.html` — outer chrome (nav, footer, htmx script). All
  full pages extend this.
- `pages/<resource>/...html` — top-level pages (list, new, edit).
- `views/<resource>/_table.html` — partial included by the list page
  (desktop `<table>` + mobile `<div>` cards).
- `components/_*.html` — stateless fragments included from one or more
  pages.

Templates render to a single HTML string and are served by
`admin::handle`. The response is post-processed by
`render_with_assets_and_csrf` (`lib.rs`), which substitutes the
`__CSRF__`, `__CSS_HASH__`, `__JS_FILE__`, and `__JS_HASH__` placeholders.

## Mutations

Every mutating action (create / update / delete / retry / reload) MUST
go through one of two mechanisms. Pick one and stick with it.

### 1. Form-POST (full page navigation)

Use only for **non-row-level** flows where the user expects a fresh
page after submit: `New`, `Edit`, login/logout, "Reload configuration".

```html
<form method="POST" action="/sites/new" class="card p-6 space-y-5">
  {% include "components/_csrf.html" %}
  <button type="submit" class="...">Create</button>
</form>
```

- `components/_csrf.html` emits `<input type="hidden" name="_csrf" value="__CSRF__">`.
- The server CSRF check (`lib.rs:99`) requires the token in the form
  body or query string and rejects with 403 otherwise.

### 2. HTMX `hx-delete` / `hx-post` (row-level swap)

Use for **row-level** mutations where the user expects the affected row
to disappear or change without a page reload. Delete is the canonical
example; "Retry" / "Toggle enabled" / etc. should follow the same
pattern.

```html
<button hx-delete="/api/sites/foo"
  hx-vals='{"_csrf": "__CSRF__"}'
  hx-confirm="Delete site foo?"
  hx-swap="delete"
  hx-target="closest tr">
  Delete
</button>
```

- The `__CSRF__` placeholder in `hx-vals` is replaced at render time
  with the session's CSRF token (`render_with_assets_and_csrf`).
- `hx-swap="delete"` removes the target element from the DOM.
- `hx-target="closest tr"` for table rows; `hx-target="closest .p-4"`
  for mobile card rows (the row wrapper has class `p-4`).
- `hx-confirm` shows a native confirm() dialog before firing the
  request — do NOT add an `onsubmit` handler, this is the equivalent.

#### Use the shared macro

For consistency, prefer the macro in
`components/_hx_delete_button.html`:

```html
{% import "components/_hx_delete_button.html" as hx %}
…
{{ hx::btn(
     url="/api/sites/foo",
     confirm="Delete site foo?",
     variant="icon",          {# or "text" for mobile full-width #}
     target="closest tr",
) }}
```

- `variant="icon"` → p-1.5 trash-icon button (desktop table row).
- `variant="text"` → full-width red "Delete" button (mobile card row).
- `target` is required — pass `closest tr` for desktop, `closest .p-4`
  for mobile, or a specific id when the button lives outside a row
  wrapper.

The macro is the source of truth for delete-button styling. If you need
to tweak the trash icon, hover color, or CSRF payload format, edit the
macro and every consumer picks up the change.

## Endpoints

The HTMX delete routes all live under `/api/<resource>/<id>` and return
an empty 200 body so `hx-swap="delete"` can drop the row:

| Resource | Route                              | Handler                  |
| -------- | ---------------------------------- | ------------------------ |
| site     | `DELETE /api/sites/{name}`         | `sites::api_handle_delete` |
| domain   | `DELETE /api/domains/{domain}`     | `domains::api_handle_delete` |
| tun      | `DELETE /api/tun/{name}`           | `tun::api_handle_delete` |
| cert     | `DELETE /api/certs/{domain}`       | `certs::api_handle_delete` |
| dns      | `DELETE /api/dns/{name}`           | `dns::api_handle_delete` |

The legacy form-POST endpoints (`POST /sites/delete`, etc.) are kept as
fallbacks during the migration window and marked with a
`// NOTE: legacy form-POST …` comment. They will be removed once every
UI migrates (tracked by issue #48).

## CSRF

CSRF is enforced for every `POST | PUT | PATCH | DELETE` request,
including the body-carrying DELETE variant used by HTMX
(`lib.rs:99-117`). The token can ride in:

- a form-encoded body field (`_csrf=...`), or
- the URL query string (`?_csrf=...`).

Either works because `lib.rs` merges body and query before checking.
HTMX `hx-vals` sends the token in the body, which is why the
server's `read_body_or_idle` path must NOT drop the DELETE body
(`serve.rs:96-98`).

## Tables (desktop + mobile)

Every list page renders twice:

1. A `<table class="hidden md:table">` block — visible at md+ breakpoints.
2. A `<div class="block md:hidden divide-y ...">` block — visible below md.

The two blocks MUST stay in sync. Same data, same row identity. Use:

- Desktop row: `<tr id="<resource>-<id>">` (id lets `hx-target` address it).
- Mobile row: `<div class="p-4">` plus `hx-target="closest .p-4"` on the button.

Empty state: render an `{% else %}` branch after the `{% for %}` loop in
each block. The mobile empty state should include a CTA that matches the
desktop's "Add one" link.

## Server data → template

`crates/admin/src/routes/<resource>/pages.rs` builds the askama struct
that the template renders against. Keep template fields minimal — the
template should format dates, status badges, and counts itself rather
than asking the route handler to pre-format strings.

## Adding a new resource

1. Add a route module under `crates/admin/src/routes/<resource>/`
   (`mod.rs`, `pages.rs`, `mutate.rs`, `views.rs`).
2. Wire `GET` / `POST` / `DELETE` (form-POST) routes in
   `crates/admin/src/lib.rs`.
3. Add the HTMX `DELETE /api/<resource>/{id}` route in `lib.rs` (same
   place as the other API routes) and a matching `api_handle_delete`
   in `mutate.rs` that returns an empty 200 body.
4. Add `<resource>/_table.html` partial + a `pages/<resource>/list.html`
   page extending `layouts/base.html`.
5. Use `{% import "components/_hx_delete_button.html" as hx %}` and the
   `hx::btn` macro for delete buttons.
6. Add e2e tests in `tests/src/admin_ui_e2e.rs`:
   - `<resource>_hx_delete_with_body_csrf_works` — happy path.
   - `<resource>_hx_delete_no_csrf_forbidden` — missing CSRF → 403.
7. Keep the legacy form-POST endpoint as a fallback and add a
   `// NOTE: legacy form-POST …` comment on its handler.