#!/bin/bash
# Guard the Docker build context.
#
# The real build (build/docker/dist.dockerfile) filters the context through
# `.dockerignore` before the compiler ever sees it.  When that filter is
# wrong the failure mode is silent and expensive:
#
#   - a *missing* input breaks the build only when that input is actually
#     read (e.g. `tailwind.config.js` — without it Tailwind scans no
#     content globs and the admin UI ships unstyled, but the build still
#     succeeds), and
#   - an *extra* input bloats every build context and, worse, can bake a
#     local secret into an image layer (`.env` holds NGX_ADMIN__PASSWORD
#     and TUN_TOKEN).
#
# This script enumerates the context Docker would actually send, using
# Docker's own matcher (a probe image with `COPY . /ctx`), and asserts:
#
#   1. the top-level entries are EXACTLY the build's allow-list,
#   2. every input the Dockerfile COPYs is present, and
#   3. every known-irrelevant path is absent.
#
# Exits non-zero with a list of violations.  Run via `make check-context`.

set -euo pipefail

# Byte-order collation everywhere: `comm` compares its inputs with the
# current locale, so a C-locale `sort` alone is not enough.
export LC_ALL=C

cd "$(dirname "$0")/.."

PROBE_DIR="$(mktemp -d)"
IMG="pangolin-ctxprobe:$$"
CTR="pangolin-ctxprobe-ctr-$$"

cleanup() {
    docker rm -f "$CTR" >/dev/null 2>&1 || true
    docker image rm -f "$IMG" >/dev/null 2>&1 || true
    rm -rf "$PROBE_DIR"
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
    echo "SKIP: docker not found — cannot probe the build context" >&2
    exit 0
fi

# `FROM scratch` + `COPY . /ctx` is the cheapest possible probe: no compiler
# runs, it just materialises the filtered context as an image layer.
cat > "$PROBE_DIR/Dockerfile" <<'EOF'
FROM scratch
COPY . /ctx
EOF

docker build --no-cache -q -f "$PROBE_DIR/Dockerfile" -t "$IMG" . >/dev/null \
    || { echo "FAILED: could not build the context probe image" >&2; exit 1; }

docker create --name "$CTR" "$IMG" true >/dev/null
# `sed -n 's|^ctx/||p'` also rewrites the `ctx/` directory entry itself into
# an empty line — drop it, or it becomes a bogus top-level entry.
docker export "$CTR" | tar -tf - \
    | sed -n 's|^ctx/||p' \
    | grep -v '/$' \
    | grep -v '^$' \
    | LC_ALL=C sort > "$PROBE_DIR/context.txt"
docker rm -f "$CTR" >/dev/null

CONTEXT="$PROBE_DIR/context.txt"
CONTEXT_FILES="$(wc -l < "$CONTEXT")"

fail=0

# ── 1. Top-level allow-list ────────────────────────────────────────────────
# The context must contain EXACTLY these top-level entries.  Anything else
# — a new top-level dir, a stray dotfile — is uploaded on every build and
# (for dotfiles) may carry a secret.  Adding a genuinely-required top-level
# input means adding it here too: the allow-list is the opt-in.
EXPECTED_TOP=(
    "Cargo.toml"
    "Cargo.lock"
    "Makefile"
    "assets"
    "build"
    "crates"
    "ngx.yml"
    "rustfmt.toml"
    "tailwind.config.js"
    "tests"
    "tun.yml"
)

actual_top="$(cut -d/ -f1 "$CONTEXT" | LC_ALL=C sort -u)"
printf '%s\n' "${EXPECTED_TOP[@]}" | LC_ALL=C sort -u > "$PROBE_DIR/expected.txt"
printf '%s\n' "$actual_top" > "$PROBE_DIR/actual.txt"

# `grep -Fxv -f` rather than `comm`: comm requires both inputs to be sorted
# *in its own collation* and only warns when they aren't, which turns the
# whole comparison into a silent no-op.  Set difference needs no ordering.
extra="$(grep -Fxv -f "$PROBE_DIR/expected.txt" "$PROBE_DIR/actual.txt" || true)"
missing="$(grep -Fxv -f "$PROBE_DIR/actual.txt" "$PROBE_DIR/expected.txt" || true)"

if [ -n "$extra" ]; then
    echo "LEAKED unexpected top-level entries in the build context:" >&2
    printf '  %s\n' "$extra" >&2
    fail=1
fi
if [ -n "$missing" ]; then
    echo "MISSING top-level entries in the build context:" >&2
    printf '  %s\n' "$missing" >&2
    fail=1
fi

# ── 2. Required inputs ─────────────────────────────────────────────────────
# Each of these is read by the build; if one goes missing the build either
# fails or (worse) silently produces a degraded binary.
REQUIRED=(
    # Cargo needs both the manifest and the lockfile.
    "Cargo.toml"
    "Cargo.lock"
    # Workspace members — `cargo` parses every member's manifest even when
    # only -p ngx -p tun are built, and `tests` is a member.
    "crates/pangolin-core/Cargo.toml"
    "crates/knownbots/Cargo.toml"
    "crates/ngx/Cargo.toml"
    "crates/tun/Cargo.toml"
    "crates/admin/Cargo.toml"
    "tests/Cargo.toml"
    # rust-embed snapshots ../../assets/ into the admin binary.
    "assets/app.js"
    "assets/tailwindcss.css"
    # Tailwind's content globs point at the admin templates.
    "tailwind.config.js"
    "crates/admin/templates/layouts/base.html"
    # `certs/` is a SOURCE directory name here, not a certificate dump.
    # Docker's .dockerignore has no "pattern without a slash matches at
    # every level" rule, so `certs/` is root-anchored — but if anyone
    # "fixes" it to `**/certs/` these vanish and the admin crate stops
    # compiling. Pin them.
    "crates/admin/src/routes/certs/mod.rs"
    "crates/admin/templates/pages/certs/list.html"
    "crates/admin/templates/views/certs/_table.html"
    # include_str! targets inside the knownbots crate.
    "crates/knownbots/conf.d/googlebot.yaml"
    # include_bytes! target for the ACME integration test.
    "crates/ngx/tests/pebble-root.pem"
    # Copied to /usr/local/cargo/config.toml (registry mirror + mold linker).
    "build/docker/cargo-config.toml"
)

for path in "${REQUIRED[@]}"; do
    if ! grep -Fxq "$path" "$CONTEXT"; then
        echo "MISSING required build input: $path" >&2
        fail=1
    fi
done

# ── 3. No hidden files or directories, at ANY depth ────────────────────────
# `.dockerignore` excludes `**/.*` — one rule for the whole class of hidden
# per-machine state (.git, .github, .codegraph, .env, .cursor, .claude,
# .vscode, .idea, .cargo, .DS_Store, …), including tools nobody has
# installed yet.  Check it as one rule rather than a list of names, so a
# newly invented dot-tool cannot slip through.
if grep -qE '(^|/)\.' "$CONTEXT"; then
    echo "LEAKED hidden file/directory in the build context:" >&2
    grep -E '(^|/)\.' "$CONTEXT" | head -10 >&2
    fail=1
fi

# ── 4. Forbidden paths ─────────────────────────────────────────────────────
# Anything here is host-side tooling, runtime state, or a local secret.
# Matched at ANY depth — a nested `.codegraph/` is just as useless as a
# root-level one, and just as expensive.
FORBIDDEN_PREFIX=(
    "target/"          # cargo target dir
    "bin/"             # host-built binaries
    "build/output/"    # previous build artefacts
    "deploy/"          # ansible playbooks
    "docs/"            # documentation
    "tasks/"           # planning notes
    "test-support/"    # unreferenced leftover
    "node_modules/"
    "coverage/"
    "test-results/"
    "logs/"            # bot-log fixtures written by `make init-data`
)

# Root-anchored only: the ROOT certs/ holds generated certificates, but
# crates/**/certs/ is SOURCE (the admin crate's cert routes and templates
# live there) and must stay in the context.
FORBIDDEN_ROOT_PREFIX=(
    "certs/"
    "certs-staging/"
)

FORBIDDEN_EXACT=(
    "assets/app.css"   # regenerated inside the builder stage
    "Thumbs.db"
)

for prefix in "${FORBIDDEN_PREFIX[@]}"; do
    if grep -qE "(^|/)${prefix}" "$CONTEXT"; then
        echo "LEAKED forbidden path: ${prefix}*" >&2
        grep -E "(^|/)${prefix}" "$CONTEXT" | head -5 >&2
        fail=1
    fi
done

for prefix in "${FORBIDDEN_ROOT_PREFIX[@]}"; do
    if grep -q "^${prefix}" "$CONTEXT"; then
        echo "LEAKED forbidden path: ${prefix}* (repo root)" >&2
        grep "^${prefix}" "$CONTEXT" | head -5 >&2
        fail=1
    fi
done

for path in "${FORBIDDEN_EXACT[@]}"; do
    if grep -Fxq "$path" "$CONTEXT"; then
        echo "LEAKED forbidden path: $path" >&2
        fail=1
    fi
done

# Markdown is never compiled in. `*.md` in .dockerignore only matches the
# repo ROOT — nested docs need `**/*.md` — so assert on every level.
if grep -q '\.md$' "$CONTEXT"; then
    echo "LEAKED markdown in context:" >&2
    grep '\.md$' "$CONTEXT" | head -5 >&2
    fail=1
fi

# ── 5. Report ──────────────────────────────────────────────────────────────
echo "docker build context: ${CONTEXT_FILES} files"
if [ "$fail" -ne 0 ]; then
    echo "✗ docker build context check FAILED" >&2
    exit 1
fi
echo "✓ docker build context OK"
