#!/usr/bin/env bash
#
# Host-side installer for the crates.io mirror.
#
# `build/cargo-mirror.toml` is the single canonical mirror block: the
# Docker image concatenates it with `build/docker/cargo-config.toml`,
# and this script injects the same file into the host's cargo config
# so the host and the image resolve the same crates from the same
# source.
#
#   ./build/cargo-mirror.sh install   # add / refresh the block (default)
#   ./build/cargo-mirror.sh off      # remove the block
#   ./build/cargo-mirror.sh status   # report the block and the active mirror
#
# The host config is a global file the user also edits by hand, so:
#
#   * the block is delimited by marker lines and contains only
#     `[source.*]` tables.  Adding a `[net]` or `[http]` table would
#     collide with the host's own and make cargo fail to parse the
#     whole file — that breaks every cargo command on the machine.
#   * the block is written at the TOP of the file, so removing it is a
#     pure prefix deletion: every byte the user wrote comes back
#     verbatim, including whether the file ended with a newline.
#   * a host config that already defines a `[source.*]` table we do
#     not manage is refused outright rather than merged.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
MIRROR_FILE="${SCRIPT_DIR}/cargo-mirror.toml"
MARK_START="# >>> pangolin cargo mirror >>>"
MARK_END="# <<< pangolin cargo mirror <<<"

CARGO_HOME_DIR="${CARGO_HOME:-${HOME}/.cargo}"
CONFIG_FILE="${CARGO_HOME_DIR}/config.toml"

# Scratch space for the rewrite.  Removed on exit, however the script
# ends — the host config is a global file and half-finished work must
# not be left lying around.
WORK="$(mktemp -d)"
trap 'rm -rf -- "${WORK}"' EXIT

die() {
    printf 'cargo-mirror: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<EOF
usage: $(basename "$0") [install|off|status]

  install   inject build/cargo-mirror.toml into ${CONFIG_FILE}
            (default; idempotent — a previous managed block is replaced)
  off       remove the managed block, leaving the rest of the file
            byte-for-byte as it was
  status    report whether the block is installed and which mirror it
            selects
EOF
}

# ── mirror introspection ───────────────────────────────────────────────────

# The name of the mirror `replace-with` selects, e.g. "rsproxy".
mirror_name() {
    awk '
        /^[[:space:]]*\[/ { section = $0 }
        section == "[source.crates-io]" && /^[[:space:]]*replace-with[[:space:]]*=/ {
            sub(/^[^=]*=[[:space:]]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ' "$MIRROR_FILE"
}

# The registry host of a mirror, e.g. "rsproxy.cn".  Cargo keys its
# registry cache directory by that host, so the directories left behind
# by a previous source can be recognised.
mirror_host() {
    local name="$1" registry
    registry="$(awk -v want="[source.${name}]" '
        $0 == want { section = 1; next }
        /^[[:space:]]*\[/ { section = 0 }
        section && /^[[:space:]]*registry[[:space:]]*=/ {
            sub(/^[^=]*=[[:space:]]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ' "$MIRROR_FILE")"
    registry="${registry#sparse+https://}"
    registry="${registry#https://}"
    registry="${registry%%/*}"
    printf '%s\n' "$registry"
}

# ── the managed block ──────────────────────────────────────────────────────

# Remove the managed block from $1, writing the remainder to $2.
#
# Byte-exact: everything outside the block survives, including the
# absence of a trailing newline on the last line.  awk cannot tell
# whether its input ended with a newline, so the flag is computed from
# the last byte of the input instead.
strip_block() {
    local src="$1" out="$2" trailing=0 last=""
    if [ -s "$src" ]; then
        last="$(tail -c 1 -- "$src" | od -An -tx1 | tr -d '[:space:]')"
        if [ "$last" = "0a" ]; then trailing=1; fi
    fi
    awk -v start="$MARK_START" -v end="$MARK_END" -v trailing="$trailing" '
        $0 == start { inblock = 1; next }
        $0 == end   { if (inblock) { inblock = 0; skipblank = 1 } next }
        inblock     { next }
        skipblank   { skipblank = 0; if ($0 == "") next; buf[++n] = $0; next }
        { buf[++n] = $0 }
        END {
            for (i = 1; i < n; i++) printf "%s\n", buf[i]
            if (n > 0) {
                printf "%s", buf[n]
                if (trailing == 1) printf "\n"
            }
        }
    ' "$src" > "$out"
}

# Replace the host config with the contents of $1.
#
# Written through a temp file in the same directory and renamed into
# place, so cargo never sees a half-written config.  The original
# file's permissions are carried over.
write_config() {
    local src="$1" tmp
    mkdir -p -- "${CARGO_HOME_DIR}"
    tmp="$(mktemp "${CARGO_HOME_DIR}/.config.toml.pangolin.XXXXXX")"
    if [ -f "${CONFIG_FILE}" ]; then
        cp -p -- "${CONFIG_FILE}" "$tmp" 2>/dev/null || true
    else
        chmod 0644 "$tmp"
    fi
    cat -- "$src" > "$tmp"
    mv -f -- "$tmp" "${CONFIG_FILE}"
}

# The registry cache is keyed by the registry host, so switching mirrors
# leaves the previous source's directories behind.  Report them and
# delete nothing — the operator decides what to do with them.
report_stale_cache() {
    local host="$1" cache="${CARGO_HOME_DIR}/registry/cache" dir base stale=0
    if [ ! -d "${cache}" ]; then return 0; fi
    for dir in "${cache}"/*/; do
        if [ ! -d "${dir}" ]; then continue; fi
        base="$(basename "${dir}")"
        case "${base}" in
            "${host}"-*) continue ;;
        esac
        if [ "$stale" -eq 0 ]; then
            printf '  stale registry cache (unused by %s, nothing was deleted):\n' "$host"
            stale=1
        fi
        printf '    %s\n' "${dir}"
    done
    return 0
}

# ── commands ───────────────────────────────────────────────────────────────

cmd_install() {
    if [ ! -f "${MIRROR_FILE}" ]; then
        die "missing ${MIRROR_FILE}"
    fi

    local base out
    base="${WORK}/base"
    out="${WORK}/new"

    if [ -f "${CONFIG_FILE}" ]; then
        strip_block "${CONFIG_FILE}" "${base}"
    else
        : > "${base}"
    fi

    # A `[source.*]` table the host already defines would collide with
    # ours: TOML forbids defining the same table twice, and a duplicate
    # makes cargo fail to parse the whole file.  Refuse instead of
    # writing something that breaks every cargo command on the machine.
    local foreign
    foreign="$(awk '/^[[:space:]]*\[source(\.|\])/ { print }' "${base}" | sort -u)"
    if [ -n "${foreign}" ]; then
        die "the host cargo config already defines a [source.*] table:
${foreign}
Remove or rename it, then re-run.  Nothing was written."
    fi

    # The managed block goes first so that removing it is a pure prefix
    # deletion (see the header comment).  The blank lines around it
    # separate it from the host's own tables; both belong to the block
    # and are removed with it.
    {
        printf '%s\n' "${MARK_START}"
        awk '1' "${MIRROR_FILE}"
        printf '\n%s\n' "${MARK_END}"
        printf '\n'
        cat -- "${base}"
    } > "${out}"

    write_config "${out}"

    local name host
    name="$(mirror_name)"
    host="$(mirror_host "${name}")"
    printf 'cargo mirror installed in %s\n' "${CONFIG_FILE}"
    printf '  active mirror: %s (%s)\n' "${name}" "${host}"
    report_stale_cache "${host}"
}

cmd_off() {
    if [ ! -f "${CONFIG_FILE}" ]; then
        printf 'no cargo config at %s — nothing to remove\n' "${CONFIG_FILE}"
        return 0
    fi

    local base
    base="${WORK}/base"
    strip_block "${CONFIG_FILE}" "${base}"

    if cmp -s -- "${base}" "${CONFIG_FILE}"; then
        printf 'no managed cargo mirror block in %s — nothing to remove\n' "${CONFIG_FILE}"
        return 0
    fi

    write_config "${base}"
    printf 'cargo mirror removed from %s\n' "${CONFIG_FILE}"
}

cmd_status() {
    if [ ! -f "${MIRROR_FILE}" ]; then
        die "missing ${MIRROR_FILE}"
    fi

    local name host present="no"
    name="$(mirror_name)"
    host="$(mirror_host "${name}")"
    if [ -f "${CONFIG_FILE}" ] && grep -qxF "${MARK_START}" "${CONFIG_FILE}"; then
        present="yes"
    fi
    printf 'cargo config  : %s\n' "${CONFIG_FILE}"
    printf 'managed block : %s\n' "${present}"
    printf 'active mirror : %s (%s)\n' "${name}" "${host}"
    printf 'mirror source : %s\n' "${MIRROR_FILE}"
}

case "${1:-install}" in
    install) cmd_install ;;
    off)     cmd_off ;;
    status)  cmd_status ;;
    -h|--help|help) usage ;;
    *)       usage >&2; exit 2 ;;
esac
