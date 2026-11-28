# syntax=docker/dockerfile:1.7
# ────────────────────────────────────────────────────────────────────────────
# The runtime images of Pangolin.
#
# Two images, one per binary:
#
#   pangolin-ngx   the gateway
#   pangolin-tun   the tunnel node
#
# Three stages: `base` carries the Debian base image and its CA bundle,
# `ngx` and `tun` add the binary, the packaged configuration and the
# runtime directories of their own program.
#
# ── why the binary is *not* compiled here ───────────────────────────────────
#
# The image is assembled from the release binaries — the very bytes the
# GitHub Release ships — and never compiles anything.  The other
# pipeline in this repository, `build/docker/dist.dockerfile`, does
# compile, but with rustc 1.98.1 while the release uses Rust 1.96
# (the version CI tests), and on a base image with a different glibc
# floor.  A container built that way would be "another binary" with
# another compatibility floor, and it would cost another 26 minutes of
# compilation.  Here the binary is the release binary.
#
# ── why the build context is not the repository root ───────────────────────
#
# The context of this build is a staging directory holding exactly four
# files (see `packaging/container/stage.sh`): the two binaries and the
# two packaged configuration templates.  `.dockerignore` therefore does
# not apply to it and does not have to change — that file governs the
# other pipeline, whose context *is* the repository root.
#
# ── why the image runs as root ─────────────────────────────────────────────
#
# The packaged configuration binds the privileged ports 80 and 443, and
# a non-root process cannot bind them without `CAP_NET_BIND_SERVICE`.
# The container is the isolation boundary here, and making the operator
# remember `--cap-add` on every `docker run` is a worse trade.
# ────────────────────────────────────────────────────────────────────────────

FROM debian:bookworm-slim AS base

# `ca-certificates` is not optional: the gateway talks HTTPS to the
# ACME directory and to the dashboard, and both need a CA bundle.
# The Debian slim image does not ship one.
RUN apt-get update -y \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# ── The gateway ─────────────────────────────────────────────────────────────
FROM base AS ngx

# The tag the release was cut from, and the commit it points at.  The
# build passes them in; the defaults keep a local build honest about
# what it does not know.
ARG IMAGE_VERSION=dev
ARG IMAGE_REVISION=unknown

LABEL org.opencontainers.image.source="https://github.com/cnlangzi/pangolin" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.revision="${IMAGE_REVISION}"

COPY pangolin-ngx /usr/bin/pangolin-ngx
# The *packaged* configuration template (`packaging/config/ngx.yml`),
# not the development one in the release archive: it carries the
# absolute paths and the privileged ports the packaged service runs
# with.  0640 is the mode the deb/rpm package gives it.
COPY ngx.yml /etc/pangolin/ngx.yml

# The directories the deb/rpm package owns, and only those.  They are
# empty skeletons — the database, the certificates and the JSONL logs
# are written by the running service.
#
# The parent directories are declared as volumes below, and Docker
# copies the content of a volume's mount point out of the image when it
# creates the volume — so the subdirectories below them have to exist
# *in the image*, or a fresh volume would come up empty.
RUN chmod 0755 /usr/bin/pangolin-ngx \
 && chmod 0640 /etc/pangolin/ngx.yml \
 && mkdir -p /var/lib/pangolin \
              /var/log/pangolin/bots \
              /var/log/pangolin/referers \
              /var/cache/pangolin/bots \
              /var/cache/pangolin/cache

# The database path (`pangolin.db`) is relative to the working
# directory, so the working directory *is* the state directory.  It
# lives in this stage and not in `base`: the two images keep their
# state apart.
WORKDIR /var/lib/pangolin

# The state directories, so that removing the container does not take
# the database, the certificates and the logs with it.
VOLUME ["/var/lib/pangolin", "/var/log/pangolin", "/var/cache/pangolin"]

# 80 / 443   the proxy
# 9081       the admin UI
# 9001       the tunnel endpoint — QUIC (udp) and the TCP fallback
#             share the port number
EXPOSE 80/tcp 443/tcp 9081/tcp 9001/udp 9001/tcp

# SIGTERM is the gateway's graceful-shutdown path, and it is also what
# `docker stop` sends.
STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/bin/pangolin-ngx"]
CMD ["--config", "/etc/pangolin/ngx.yml"]

# ── The tunnel node ─────────────────────────────────────────────────────────
FROM base AS tun

ARG IMAGE_VERSION=dev
ARG IMAGE_REVISION=unknown

LABEL org.opencontainers.image.source="https://github.com/cnlangzi/pangolin" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.revision="${IMAGE_REVISION}"

COPY pangolin-tun /usr/bin/pangolin-tun
# The *packaged* configuration template (`packaging/config/tun.yml`):
# it names the absolute paths of the pin file and the QUIC-failure
# record, both of which live under the tunnel node's own directory.
COPY tun.yml /etc/pangolin/tun.yml

# The tunnel node keeps no logs on disk — its log lines go to stderr
# and from there into the container's log.  What it keeps is the
# trust-on-first-use pin file and the record of the last failed QUIC
# attempt, both under /var/lib/pangolin-tun.
RUN chmod 0755 /usr/bin/pangolin-tun \
 && chmod 0640 /etc/pangolin/tun.yml \
 && mkdir -p /var/lib/pangolin-tun

WORKDIR /var/lib/pangolin-tun

VOLUME ["/var/lib/pangolin-tun"]

# 9001 is the tunnel port — QUIC over UDP and the WebSocket/TCP
# fallback over TCP.  The tunnel node *dials* the gateway's
# `tunnel.addr` from an ephemeral local port, so it never listens
# here; the port is declared for symmetry with the gateway image,
# which does bind it.
EXPOSE 9001/udp 9001/tcp

STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/bin/pangolin-tun"]
CMD ["--config", "/etc/pangolin/tun.yml"]
