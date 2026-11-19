#!/bin/sh
# build-packages.sh — turn the release binaries into the eight
# packages.
#
# For one architecture this produces four files: a `.deb` and an
# `.rpm` for each of the two packages.
#
# Usage:
#
#   build-packages.sh --tag-version <v1.2.3> --arch <deb-arch> \
#                     --rpm-arch <rpm-arch> --bin-dir <dir> \
#                     --out-dir <dir> [--revision <rev>]
#
# `--tag-version` is the version *without* the leading `v` — the same
# string the release workflow reads out of the pushed tag.  The
# package version is derived from it:
#
#   0.3.0        →  0.3.0
#   1.2.3-rc1    →  1.2.3~rc1
#
# The `~` is what makes a pre-release sort *before* the release it
# precedes in Debian's version order (`1.2.3~rc1-1` < `1.2.3-1`), and
# rpm's version comparison understands `~` the same way.  A `-` may
# not appear in the upstream part of a Debian version, so a
# pre-release that carries one (`1.2.3-rc-1`) has it folded into a
# `+`.
#
# The glibc floor in the dependency list is *measured*, per binary:
# the highest `GLIBC_x.y` symbol version the binary records is read
# out of it with `objdump`.  A floor written down by hand goes stale
# the moment the toolchain or a dependency changes, and a stale floor
# is worse than no floor at all — it either keeps the package off a
# host that could run it, or lets it onto a host that cannot.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
packaging_dir="$script_dir"

tag_version=""
revision=1
arch=""
rpm_arch=""
bin_dir=""
out_dir=""
nfpm_bin="${NFPM:-nfpm}"

usage() {
    echo "usage: build-packages.sh --tag-version <ver> --arch <deb-arch>" >&2
    echo "                          --rpm-arch <rpm-arch> --bin-dir <dir>" >&2
    echo "                          --out-dir <dir> [--revision <rev>]" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --tag-version)
            tag_version="$2"
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
        --rpm-arch)
            rpm_arch="$2"
            shift 2
            ;;
        --bin-dir)
            bin_dir="$2"
            shift 2
            ;;
        --out-dir)
            out_dir="$2"
            shift 2
            ;;
        -h | --help)
            usage
            ;;
        *)
            echo "build-packages.sh: unknown argument '$1'" >&2
            usage
            ;;
    esac
done

for required in tag_version arch rpm_arch bin_dir out_dir; do
    eval "value=\$$required"
    if [ -z "$value" ]; then
        echo "build-packages.sh: missing --$required" >&2
        usage
    fi
done

if ! command -v "$nfpm_bin" >/dev/null 2>&1; then
    echo "build-packages.sh: '$nfpm_bin' is not on PATH — the packages cannot be built" >&2
    exit 1
fi

if ! command -v objdump >/dev/null 2>&1; then
    echo "build-packages.sh: objdump is not on PATH — the glibc floor cannot be measured" >&2
    exit 1
fi

# ── the version ─────────────────────────────────────────────────────────────

case "$tag_version" in
    [0-9]*)
        ;;
    *)
        echo "build-packages.sh: '$tag_version' does not look like a version" >&2
        exit 1
        ;;
esac

core=${tag_version%%-*}
rest=${tag_version#*-}
if [ "$rest" = "$tag_version" ]; then
    package_version=$core
else
    rest=$(printf '%s' "$rest" | tr '-' '+')
    package_version="$core~$rest"
fi

mkdir -p "$out_dir"
out_dir=$(CDPATH= cd -- "$out_dir" && pwd)

# ── the glibc floor ─────────────────────────────────────────────────────────

# The highest `GLIBC_x.y` symbol version the binary records.
#
# `GLIBC_PRIVATE` is not a version — the digit in the pattern keeps it
# out of the comparison, and `sort -V` orders `2.34` above `2.2.5`,
# which a plain string sort would not.
measure_glibc() {
    binary="$1"
    version=$(objdump -T "$binary" \
        | grep -o 'GLIBC_[0-9][0-9.]*' \
        | sort -V \
        | tail -1 \
        | sed 's/^GLIBC_//')
    if [ -z "$version" ]; then
        echo "build-packages.sh: could not read a GLIBC symbol version out of $binary" >&2
        exit 1
    fi
    printf '%s\n' "$version"
}

# ── the packages ────────────────────────────────────────────────────────────

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

for package in pangolin-ngx pangolin-tun; do
    binary="$bin_dir/$package"
    if [ ! -f "$binary" ]; then
        echo "build-packages.sh: $binary is missing" >&2
        exit 1
    fi

    glibc=$(measure_glibc "$binary")
    echo "$package: glibc floor $glibc"

    config="$work/nfpm-$package-$arch.yaml"
    sh "$packaging_dir/render-nfpm.sh" \
        --package "$package" \
        --version "$package_version" \
        --revision "$revision" \
        --arch "$arch" \
        --rpm-arch "$rpm_arch" \
        --glibc "$glibc" \
        --binary "$binary" \
        --out "$config" >/dev/null

    deb_target="$out_dir/${package}_${package_version}-${revision}_${arch}.deb"
    rpm_target="$out_dir/${package}-${package_version}-${revision}.${rpm_arch}.rpm"

    "$nfpm_bin" package --packager deb --config "$config" --target "$deb_target"
    "$nfpm_bin" package --packager rpm --config "$config" --target "$rpm_target"

    for produced in "$deb_target" "$rpm_target"; do
        if [ ! -f "$produced" ]; then
            echo "build-packages.sh: nfpm did not produce $produced" >&2
            exit 1
        fi
    done
    echo "built $(basename "$deb_target") and $(basename "$rpm_target")"
done

ls -l "$out_dir"
