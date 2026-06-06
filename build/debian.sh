#!/bin/bash
set -e

cd "$(dirname "$0")/.."

docker build --progress plain -f ./build/docker/debian.dockerfile . -t pangolin-debian:latest