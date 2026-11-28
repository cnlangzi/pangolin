#!/bin/sh
# stage.sh — assemble the build context of the runtime images.
#
# Usage:
#
#   stage.sh --bin-dir <dir> --out-dir <dir>
#
# The build context of `packaging/container/runtime.dockerfile` is a
# staging directory holding exactly four files:
#
#   pangolin-ngx   0755   the release binary
#   pangolin-tun   0755   the release binary
#   ngx.yml        0640   packaging/config/ngx.yml
#   tun.yml        0640   packaging/config/tun.yml
#
# Two decisions live here:
#
#   * the context is *not* the repository root.  The runtime image
#     needs four files, so the context holds four files, and
#     `.dockerignore` — which governs the other pipeline, the one in
#     build/docker/dist.dockerfile — does not have to change.
#
#   * the configuration templates come from `packaging/config/`, not
#     from the release archive.  The archive carries the repository's
#     *development* configuration (relative paths, the ports 8000 and
#     8443); the packaged templates carry the absolute paths and the
#     privileged ports the packaged service runs with.  An image that
#     shipped the development configuration would come up on the wrong
#     ports, with paths that follow the working directory.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

bin_dir=""
out_dir=""

usage() {
    echo "usage: stage.sh --bin-dir <dir> --out-dir <dir>" >&2
    exit 2
}

help() {
    echo "usage: stage.sh --bin-dir <dir> --out-dir <dir>"
    echo
    echo "Assemble the build context of the runtime images: the two binaries"
    echo "and the two packaged configuration templates, and nothing else."
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --bin-dir)
            bin_dir="$2"
            shift 2
            ;;
        --out-dir)
            out_dir="$2"
            shift 2
            ;;
        -h | --help)
            help
            ;;
        *)
            echo "stage.sh: unknown argument '$1'" >&2
            usage
            ;;
    esac
done

if [ -z "$bin_dir" ]; then
    echo "stage.sh: missing --bin-dir" >&2
    usage
fi
if [ -z "$out_dir" ]; then
    echo "stage.sh: missing --out-dir" >&2
    usage
fi

if [ ! -d "$bin_dir" ]; then
    echo "stage.sh: $bin_dir is not a directory" >&2
    exit 1
fi

mkdir -p "$out_dir"
out_dir=$(CDPATH= cd -- "$out_dir" && pwd)
bin_dir=$(CDPATH= cd -- "$bin_dir" && pwd)

for binary in pangolin-ngx pangolin-tun; do
    if [ ! -f "$bin_dir/$binary" ]; then
        echo "stage.sh: $bin_dir/$binary is missing — the build context needs the" \
             "release binaries (run 'make build-dist' for a local build)" >&2
        exit 1
    fi
    install -m 0755 "$bin_dir/$binary" "$out_dir/$binary"
done

# 0640, the mode the deb/rpm package gives the conffile: the files
# carry the admin password and the tunnel token.
install -m 0640 "$repo_root/packaging/config/ngx.yml" "$out_dir/ngx.yml"
install -m 0640 "$repo_root/packaging/config/tun.yml" "$out_dir/tun.yml"

# The context must hold these four files and nothing else: a stale
# file from an earlier run would be baked into the image, and the
# Dockerfile copies by name, so it would not even be noticed there.
staged=$(find "$out_dir" -mindepth 1 -maxdepth 1 | wc -l)
if [ "$staged" -ne 4 ]; then
    echo "stage.sh: $out_dir holds $staged entries, expected the 4 files of the" \
         "build context — empty the directory first" >&2
    exit 1
fi

ls -l "$out_dir"
