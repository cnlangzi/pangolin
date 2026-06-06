#!/bin/bash
set -e

cd "$(dirname "$0")/.."

mkdir -p ./build/output

docker build --progress plain \
  --target export-stage \
  -f ./build/docker/dist.dockerfile . \
  -o "type=local,dest=./build/output/"

echo "=== Build output ==="
ls -lh ./build/output/