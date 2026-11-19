#!/bin/sh
# preremove — stop the service and take it out of the boot sequence.
#
# Runs *before* the package's files are removed.  Stopping first
# matters: the binary is unlinked a moment later, and a running
# gateway that loses its executable under its feet is worse than a
# clean stop.
#
# An upgrade is not a removal.  dpkg calls `prerm` with `upgrade`,
# rpm's `%preun` with `1`; in both cases the service has to keep
# running so the new package can take over without an outage.
#
# `PANGOLIN_ROOT` prefixes every path the script touches — see
# `postinstall.sh`.
#
# POSIX `sh`: dpkg runs maintainer scripts with `/bin/sh`, and so does
# rpm's scriptlet interpreter.

set -e

root="${PANGOLIN_ROOT:-}"

case "${1:-}" in
    upgrade | failed-upgrade | 1)
        # An upgrade — leave the service running.
        exit 0
        ;;
esac

say() {
    echo "pangolin: $*"
}

unit_installed() {
    for dir in "$root/usr/lib/systemd/system" "$root/lib/systemd/system" "$root/etc/systemd/system"; do
        if [ -f "$dir/$1" ]; then
            return 0
        fi
    done
    return 1
}

stop_and_disable() {
    unit="$1"
    unit_installed "$unit" || return 0

    if command -v systemctl >/dev/null 2>&1; then
        systemctl stop "$unit" >/dev/null 2>&1 || true
        systemctl disable "$unit" >/dev/null 2>&1 || true
    fi

    if [ -x "$root/usr/bin/deb-systemd-helper" ]; then
        "$root/usr/bin/deb-systemd-helper" disable "$unit" >/dev/null 2>&1 || true
    fi

    say "stopped and disabled $unit"
}

stop_and_disable pangolin-ngx.service
stop_and_disable pangolin-tun.service

exit 0
