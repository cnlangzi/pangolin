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
#   * the block is appended at the END of the file.  It has to be:
#     TOML binds a table-less key to the most recent table header, so
#     a block in front of the host's own tables would swallow the
#     host's root-level keys (`credential-provider` and friends) and
#     silently drop them.  Appending keeps every host key where it
#     was.  Removing the block is then the inverse operation, and it
#     restores the original bytes exactly — including whether the file
#     ended with a newline.
#   * a host config that already defines a `[source.*]` table we do
#     not manage is refused outright rather than merged.
#   * a config whose markers are malformed (an end marker without a
#     start marker, a start marker without an end marker, or content
#     after the block) is refused outright rather than half-rewritten.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
MIRROR_FILE="${SCRIPT_DIR}/cargo-mirror.toml"
MARK_START="# >>> pangolin cargo mirror >>>"
MARK_END="# <<< pangolin cargo mirror <<<"

CARGO_HOME_DIR="${CARGO_HOME:-${HOME}/.cargo}"

# Quote characters the mirror file may use around a TOML string.
DQ='"'
SQ="'"

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

# ── paths ──────────────────────────────────────────────────────────────────

# Resolve a symlink chain to the file it points at.
#
# The config is rewritten through a temp file and renamed into place.
# On a symlinked config that would replace the *link* with a regular
# file, silently disconnecting a dotfiles-managed config — so follow
# the link and write to its target instead.
resolve_path() {
    local path="$1" target guard=0
    while [ -L "${path}" ] && [ "${guard}" -lt 40 ]; do
        target="$(readlink "${path}")"
        case "${target}" in
            /*) path="${target}" ;;
            *)  path="$(dirname "${path}")/${target}" ;;
        esac
        guard=$((guard + 1))
    done
    printf '%s\n' "${path}"
}

CONFIG_FILE="$(resolve_path "${CARGO_HOME_DIR}/config.toml")"

# ── mirror introspection ───────────────────────────────────────────────────

# Read the mirror file and print "<name> <host>" for the mirror
# `replace-with` selects, e.g. "rsproxy rsproxy.cn".
#
# Exit codes: 4 no replace-with, 5 the selected mirror is not defined,
# 6 the selected mirror has no registry URL.
mirror_info() {
    local out status=0
    out="$(awk -v dq="${DQ}" -v sq="${SQ}" '
        # Strip the quotes off a TOML string value.  Both quote styles
        # are legal, and the mirror file is meant to be hand-edited.
        function unquote(s) {
            sub(/^[^=]*=[[:space:]]*/, "", s)
            if (substr(s, 1, 1) == dq || substr(s, 1, 1) == sq) s = substr(s, 2)
            p = index(s, dq)
            q = index(s, sq)
            if (p > 0 && (q == 0 || p < q)) s = substr(s, 1, p - 1)
            else if (q > 0) s = substr(s, 1, q - 1)
            return s
        }
        {
            header = $0
            gsub(/[[:space:]]/, "", header)
            if (header ~ /^\[/) {
                current = ""
                if (header ~ /^\[source\.[^]]+\]$/) {
                    current = header
                    sub(/^\[source\./, "", current)
                    sub(/\]$/, "", current)
                    tables[current] = 1
                }
                next
            }
            if (current == "") next
            if (current == "crates-io" && $0 ~ /^[[:space:]]*replace-with[[:space:]]*=/) {
                name = unquote($0)
                next
            }
            if ($0 ~ /^[[:space:]]*registry[[:space:]]*=/) {
                registry[current] = unquote($0)
            }
        }
        END {
            if (name == "") exit 4
            if (!(name in tables)) exit 5
            if (registry[name] == "") exit 6
            url = registry[name]
            sub(/^sparse\+/, "", url)
            sub(/^https?:\/\//, "", url)
            sub(/\/.*$/, "", url)
            print name, url
        }
    ' "${MIRROR_FILE}")" || status=$?

    case "${status}" in
        0) ;;
        4) die "${MIRROR_FILE}: no replace-with under [source.crates-io]" ;;
        5) die "${MIRROR_FILE}: replace-with names a mirror that is not defined" ;;
        6) die "${MIRROR_FILE}: the selected mirror has no registry URL" ;;
        *) die "failed to parse ${MIRROR_FILE}" ;;
    esac

    printf '%s\n' "${out}"
}

# ── the managed block ──────────────────────────────────────────────────────

# Remove the managed block from $1, writing the remainder to $2.
#
# The block is the tail of the file, preceded by exactly one newline
# that belongs to it — so when a block is present the remainder is the
# content before it minus that final newline.  With no block present
# the file is passed through unchanged, trailing newline included.
# awk cannot tell whether its input ended with a newline, so the flag
# is computed from the last byte of the input.
#
# Returns 3 when the markers are malformed — the caller must not
# write anything in that case, because a half-understood block means
# the file's real content is unknown.
strip_block() {
    local src="$1" out="$2" status=0 trailing=0 last=""
    if [ -s "${src}" ]; then
        last="$(tail -c 1 -- "${src}" | od -An -tx1 | tr -d '[:space:]')"
        if [ "$last" = "0a" ]; then trailing=1; fi
    fi
    awk -v start="${MARK_START}" -v end="${MARK_END}" -v trailing="${trailing}" '
        $0 == start {
            if (inblock && malformed == "") malformed = "a second start marker"
            inblock = 1
            next
        }
        $0 == end {
            if (!inblock) {
                if (malformed == "") malformed = "an end marker without a start marker"
                next
            }
            inblock = 0
            closed = 1
            found = 1
            next
        }
        inblock { next }
        closed {
            if (malformed == "") malformed = "content after the managed block"
            next
        }
        { buf[++n] = $0 }
        END {
            if (inblock && malformed == "") malformed = "an unterminated managed block"
            if (malformed != "") exit 3
            for (i = 1; i < n; i++) printf "%s\n", buf[i]
            if (n > 0) {
                printf "%s", buf[n]
                # Without a block the file must survive byte-for-byte;
                # with one, the newline before the block belongs to
                # the block and goes with it.
                if (!found && trailing == 1) printf "\n"
            }
        }
    ' "${src}" > "${out}" || status=$?

    if [ "${status}" -ne 0 ]; then
        return "${status}"
    fi
    return 0
}

# Replace the host config with the contents of $1.
#
# Written through a temp file in the same directory and renamed into
# place, so cargo never sees a half-written config.  The original
# file's permissions are carried over.
write_config() {
    local src="$1" tmp
    mkdir -p -- "$(dirname -- "${CONFIG_FILE}")"
    tmp="$(mktemp "$(dirname -- "${CONFIG_FILE}")/.config.toml.pangolin.XXXXXX")"
    if [ -f "${CONFIG_FILE}" ]; then
        cp -p -- "${CONFIG_FILE}" "$tmp" 2>/dev/null || true
    else
        chmod 0644 "$tmp"
    fi
    cat -- "${src}" > "$tmp"
    mv -f -- "$tmp" "${CONFIG_FILE}"
}

# The registry cache, index and extracted sources are all keyed by the
# registry host, so switching mirrors leaves the previous source's
# directories behind.  Report them and delete nothing — the operator
# decides what to do with them.
report_stale_cache() {
    local host="$1" kind root dir base stale=0
    for kind in cache index src; do
        root="${CARGO_HOME_DIR}/registry/${kind}"
        if [ ! -d "${root}" ]; then continue; fi
        for dir in "${root}"/*/; do
            if [ ! -d "${dir}" ]; then continue; fi
            base="$(basename "${dir}")"
            case "${base}" in
                "${host}"-*) continue ;;
            esac
            if [ "$stale" -eq 0 ]; then
                printf '  stale registry directories (unused by %s, nothing was deleted):\n' "$host"
                stale=1
            fi
            printf '    %s\n' "${dir}"
        done
    done
    return 0
}

# ── commands ───────────────────────────────────────────────────────────────

# The mirror file is injected verbatim, so it must end with a newline
# — otherwise MARK_END would be glued to its last line.
check_mirror_file() {
    if [ ! -f "${MIRROR_FILE}" ]; then
        die "missing ${MIRROR_FILE}"
    fi
    if [ -s "${MIRROR_FILE}" ]; then
        local last
        last="$(tail -c 1 -- "${MIRROR_FILE}" | od -An -tx1 | tr -d '[:space:]')"
        if [ "$last" != "0a" ]; then
            die "${MIRROR_FILE} must end with a newline"
        fi
    fi
}

cmd_install() {
    check_mirror_file

    local info name host
    info="$(mirror_info)"
    name="${info%% *}"
    host="${info#* }"

    local base out rc=0
    base="${WORK}/base"
    out="${WORK}/new"

    if [ -f "${CONFIG_FILE}" ]; then
        strip_block "${CONFIG_FILE}" "${base}" || rc=$?
        if [ "$rc" -ne 0 ]; then
            die "the host cargo config has a malformed managed block (${CONFIG_FILE}).
Fix the marker lines by hand — nothing was written."
        fi
    else
        : > "${base}"
    fi

    # A `[source.*]` table the host already defines would collide with
    # ours: TOML forbids defining the same table twice, and a duplicate
    # makes cargo fail to parse the whole file.  Refuse instead of
    # writing something that breaks every cargo command on the machine.
    # TOML allows whitespace inside the brackets, so `[ source.x ]` is
    # the same table as `[source.x]`.
    local foreign
    foreign="$(awk '{ line = $0; gsub(/[[:space:]]/, "", line); if (line ~ /^\[source(\.|\])/) print }' "${base}" | sort -u)"
    if [ -n "${foreign}" ]; then
        die "the host cargo config already defines a [source.*] table:
${foreign}
Remove or rename it, then re-run.  Nothing was written."
    fi

    # The managed block goes at the END of the file (see the header
    # comment): a block in front of the host's own tables would swallow
    # the host's table-less keys.
    {
        if [ -s "${base}" ]; then
            cat -- "${base}"
            printf '\n'
        fi
        printf '%s\n' "${MARK_START}"
        cat -- "${MIRROR_FILE}"
        printf '\n%s\n' "${MARK_END}"
    } > "${out}"

    write_config "${out}"

    printf 'cargo mirror installed in %s\n' "${CONFIG_FILE}"
    printf '  active mirror: %s (%s)\n' "${name}" "${host}"
    report_stale_cache "${host}"
}

cmd_off() {
    if [ ! -f "${CONFIG_FILE}" ]; then
        printf 'no cargo config at %s — nothing to remove\n' "${CONFIG_FILE}"
        return 0
    fi

    local base rc=0
    base="${WORK}/base"
    strip_block "${CONFIG_FILE}" "${base}" || rc=$?
    if [ "$rc" -ne 0 ]; then
        die "the host cargo config has a malformed managed block (${CONFIG_FILE}).
Fix the marker lines by hand — nothing was written."
    fi

    if cmp -s -- "${base}" "${CONFIG_FILE}"; then
        printf 'no managed cargo mirror block in %s — nothing to remove\n' "${CONFIG_FILE}"
        return 0
    fi

    if [ ! -s "${base}" ]; then
        # An empty config and no config are the same thing to cargo —
        # do not leave a 0-byte file behind on a global path.
        rm -f -- "${CONFIG_FILE}"
        printf 'cargo mirror removed from %s (nothing was left of the config)\n' "${CONFIG_FILE}"
        return 0
    fi

    write_config "${base}"
    printf 'cargo mirror removed from %s\n' "${CONFIG_FILE}"
}

cmd_status() {
    check_mirror_file

    local info name host present="no"
    info="$(mirror_info)"
    name="${info%% *}"
    host="${info#* }"

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
