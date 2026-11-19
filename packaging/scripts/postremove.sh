#!/bin/sh
# postremove — the last word on the way out.
#
# The package's files are gone by now.  What is left on the disk is
# the *runtime state*: the SQLite database, the ACME certificates, the
# JSONL logs and the tunnel node's pin file.  None of that is in the
# package, and none of it is removed here — deleting an operator's
# database and certificates because a package was uninstalled is not
# something a package manager gets to decide on its own.  The
# directories are named instead, so the operator knows what is left.
#
# An upgrade is not a removal (dpkg calls `postrm` with `upgrade`,
# rpm's `%postun` with `1`), and on an upgrade there is nothing to
# report.
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
        exit 0
        ;;
esac

say() {
    echo "pangolin: $*"
}

# dpkg's `purge` is the "remove everything, including the
# configuration" step: a plain `remove` deliberately leaves the
# conffiles alone, so the operator's edited configuration survives a
# reinstall.  The configuration carries the admin password and the
# ACME account, so it goes only when the operator asks for it.
if [ "${1:-}" = "purge" ]; then
    for config in "$root/etc/pangolin/ngx.yml" "$root/etc/pangolin/tun.yml"; do
        if [ -f "$config" ]; then
            rm -f "$config" 2>/dev/null || true
            say "removed ${config#"$root"}"
        fi
    done
fi

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
}

stop_and_disable pangolin-ngx.service
stop_and_disable pangolin-tun.service

# The two state directories hold different things, and the operator
# has to be told which is which: the gateway's holds the database and
# the ACME certificates, the tunnel node's holds the trust-on-first-use
# pin file and the record of the last failed QUIC attempt.  The tunnel
# node writes no logs to disk at all.
if [ -d "$root/var/lib/pangolin" ]; then
    say "/var/lib/pangolin was left in place — it holds the database and the"
    say "ACME certificates.  Remove it by hand once you are sure you do not"
    say "need them any more."
fi

if [ -d "$root/var/lib/pangolin-tun" ]; then
    say "/var/lib/pangolin-tun was left in place — it holds the trust-on-first-use"
    say "pin file and the record of the last failed QUIC attempt.  Remove it by"
    say "hand once you are sure you do not need them any more."
fi

exit 0
