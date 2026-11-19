#!/bin/sh
# render-nfpm.sh — render `packaging/nfpm.yaml` into one concrete
# nfpm configuration.
#
# The template carries `@@…@@` placeholders; this script fills them in.
# It is plain `sh` on purpose: no nfpm, no Go toolchain, no network —
# the renderer is what the regression test drives, so it has to run
# anywhere the test suite runs.
#
# Usage:
#
#   render-nfpm.sh --package <name> --version <ver> --revision <rev> \
#                  --arch <arch> --glibc <x.y> --binary <path> \
#                  --out <path>
#
# `--package` is `pangolin-ngx` or `pangolin-tun`; it selects the
# file list from `packaging/contents/<package>.yaml` and the
# configuration template and the unit from `packaging/config/` and
# `packaging/systemd/`.
#
# The architecture is written once: nfpm derives the rpm's own
# nomenclature from it and names both output files after it.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
packaging_dir="$script_dir"

package=""
version=""
revision=""
arch=""
glibc=""
binary=""
out=""

usage() {
    echo "usage: render-nfpm.sh --package <name> --version <ver> --revision <rev>" >&2
    echo "                       --arch <arch> --glibc <x.y> --binary <path>" >&2
    echo "                       --out <path>" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --package)
            package="$2"
            shift 2
            ;;
        --version)
            version="$2"
            shift 2
            ;;
        --revision)
            revision="$2"
            shift 2
            ;;
        --arch)
            arch="$2"
            shift 2
            ;;
        --glibc)
            glibc="$2"
            shift 2
            ;;
        --binary)
            binary="$2"
            shift 2
            ;;
        --out)
            out="$2"
            shift 2
            ;;
        -h | --help)
            usage
            ;;
        *)
            echo "render-nfpm.sh: unknown argument '$1'" >&2
            usage
            ;;
    esac
done

# The flags and the variables they fill, so that the "you forgot one"
# message names the flag the caller actually typed.
for required in package:package version:version revision:revision \
                arch:arch glibc:glibc binary:binary out:out
do
    flag=${required%%:*}
    variable=${required#*:}
    eval "value=\$$variable"
    if [ -z "$value" ]; then
        echo "render-nfpm.sh: missing --$flag" >&2
        usage
    fi
done

case "$package" in
    pangolin-ngx | pangolin-tun) ;;
    *)
        echo "render-nfpm.sh: unknown package '$package' (expected pangolin-ngx or pangolin-tun)" >&2
        exit 1
        ;;
esac

case "$package" in
    pangolin-ngx)
        config_name=ngx
        description="Pangolin gateway — reverse proxy with crawler logging, rate limiting and referral attribution"
        ;;
    pangolin-tun)
        config_name=tun
        description="Pangolin tunnel node — connects customer traffic to a Pangolin gateway"
        ;;
esac

# Escape the characters that are special to `sed`'s replacement:
# a backslash, an ampersand (which stands for the whole match) and
# the `|` delimiter itself.
escape() {
    printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}

substitute() {
    # substitute <file> <placeholder> <value>
    #
    # Comment lines are left alone: the template's own header
    # documents the placeholders by name, and substituting there would
    # turn the documentation into nonsense.
    sed -e "/^[[:space:]]*#/! s|$(escape "$2")|$(escape "$3")|g" "$1"
}

# ── the file list ───────────────────────────────────────────────────────────

contents_src="$packaging_dir/contents/$package.yaml"
config_src="$packaging_dir/config/$config_name.yml"
unit_src="$packaging_dir/systemd/$package.service"
tmpfiles_src="$packaging_dir/tmpfiles.d/pangolin.conf"
scripts_dir="$packaging_dir/scripts"

for required_file in "$contents_src" "$config_src" "$unit_src"; do
    if [ ! -f "$required_file" ]; then
        echo "render-nfpm.sh: missing $required_file" >&2
        exit 1
    fi
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

contents="$work/contents.yaml"

# The tunnel node has no tmpfiles rule, so its file list does not
# carry the placeholder at all — substituting it there would be a
# no-op, and reading it for a file that is not shipped would be
# misleading.
cp "$contents_src" "$contents"
substitute "$contents" "@@BINARY@@" "$binary" > "$contents.tmp" && mv "$contents.tmp" "$contents"
substitute "$contents" "@@CONFIG@@" "$config_src" > "$contents.tmp" && mv "$contents.tmp" "$contents"
substitute "$contents" "@@UNIT@@" "$unit_src" > "$contents.tmp" && mv "$contents.tmp" "$contents"
if grep -q '@@TMPFILES@@' "$contents"; then
    substitute "$contents" "@@TMPFILES@@" "$tmpfiles_src" > "$contents.tmp" && mv "$contents.tmp" "$contents"
fi

# The list is a YAML sequence; it has to sit under `contents:` at the
# template's indentation level.  The manifest's own header comment
# documents the *source* file, so it does not travel into the
# rendered configuration.
{
    awk '
        # Skip the header: everything up to the first list entry.
        !started && /^- / { started = 1 }
        started { print }
    ' "$contents" | sed -e 's/[[:space:]]*$//' -e '/^$/d' | sed -e 's/^/  /'
} > "$contents.indented"

# ── the configuration itself ────────────────────────────────────────────────

cp "$packaging_dir/nfpm.yaml" "$work/nfpm.yaml"
for pair in \
    "PACKAGE:$package" \
    "DESCRIPTION:$description" \
    "VERSION:$version" \
    "REVISION:$revision" \
    "ARCH:$arch" \
    "GLIBC:$glibc" \
    "SCRIPTS:$scripts_dir"
do
    placeholder=${pair%%:*}
    value=${pair#*:}
    substitute "$work/nfpm.yaml" "@@$placeholder@@" "$value" > "$work/nfpm.tmp" && mv "$work/nfpm.tmp" "$work/nfpm.yaml"
done

# The file list goes in last: it is a multi-line block, not a single
# value, and `sed`'s replacement cannot span lines.
awk -v list="$contents.indented" '
    /^\@\@CONTENTS\@\@$/ {
        while ((getline line < list) > 0) print line
        close(list)
        next
    }
    { print }
' "$work/nfpm.yaml" > "$out"

# A placeholder that survived the substitution would silently become a
# literal `@@NAME@@` in the package metadata.  The template's own
# header mentions the placeholders in comments, so only the
# configuration itself is checked.
if grep -v '^[[:space:]]*#' "$out" | grep -q '@@'; then
    echo "render-nfpm.sh: the rendered configuration still carries a placeholder:" >&2
    grep -n '@@' "$out" >&2
    exit 1
fi

echo "rendered $out"
