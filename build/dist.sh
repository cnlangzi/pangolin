#!/bin/bash
set -e

cd "$(dirname "$0")/.."

mkdir -p ./build/output

# `--network=host` makes the build steps resolve DNS through the host's
# systemd-resolved stub (127.0.0.53) instead of the LAN router
# (192.168.31.1) that Docker hands to containers on the default bridge.
# The router drops DNS replies under load and the build dies with
# `read udp ...: i/o timeout`; the local stub caches and answers in
# parallel.  Only RUN steps are affected — pulling the base image and
# COPYing the build context are unchanged.
docker build --progress plain \
  --network=host \
  --target export-stage \
  -f ./build/docker/dist.dockerfile . \
  -o "type=local,dest=./build/output/"

echo "=== Build output ==="
ls -lh ./build/output/