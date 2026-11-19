#!/bin/sh
# postinstall — prepare the directories, register the unit, tell the
# operator what is left to do.
#
# The script never starts anything.  A package is routinely installed
# by an unattended provisioning run, and a gateway that grabs 80/443
# the moment it is installed is a liability, not a feature.  It also
# never enables the tunnel node: a tun whose `(name, token)` row does
# not exist in the gateway's admin UI yet is rejected at the handshake
# and the unit would restart-loop.
#
# The script is package-agnostic on purpose — the same four scripts
# serve `pangolin-ngx` and `pangolin-tun`.  It tells the two apart by
# the unit files the package just laid down:
#
#   * `/usr/lib/systemd/system/pangolin-ngx.service` present → the
#     gateway's directories are prepared and the gateway is enabled;
#   * `/usr/lib/systemd/system/pangolin-tun.service` present → the
#     tunnel node's directory is prepared and nothing is enabled.
#
# Installing both packages therefore works: each script prepares its
# own directories and the gateway ends up enabled exactly once.
#
# `PANGOLIN_ROOT` prefixes every path the script touches.  It is empty
# in production — dpkg and rpm run the scriptlets against the real
# filesystem — and it exists so the regression test can drive the
# script against a throwaway directory without root.
#
# POSIX `sh`: dpkg runs maintainer scripts with `/bin/sh`, and so does
# rpm's scriptlet interpreter.

root="${PANGOLIN_ROOT:-}"

PANGOLIN_USER=pangolin
PANGOLIN_GROUP=pangolin

NGX_UNIT=pangolin-ngx.service
TUN_UNIT=pangolin-tun.service

say() {
    echo "pangolin: $*"
}

warn() {
    echo "pangolin: WARNING: $*" >&2
}

# The path of an installed unit file, or nothing.
unit_path() {
    for dir in "$root/usr/lib/systemd/system" "$root/lib/systemd/system" "$root/etc/systemd/system"; do
        if [ -f "$dir/$1" ]; then
            echo "$dir/$1"
            return 0
        fi
    done
    return 1
}

# Create a state directory owned by the service user.
#
# The package ships the directory as well, but the ownership is set
# here too: a directory that dpkg created while the `pangolin` group
# did not exist yet, or one the operator replaced, ends up with the
# wrong owner otherwise — and the service would then fail to write
# its database.
ensure_dir() {
    dir="$1"
    if [ ! -d "$dir" ]; then
        mkdir -p "$dir" 2>/dev/null || {
            warn "could not create $dir"
            return 1
        }
    fi
    chown "$PANGOLIN_USER:$PANGOLIN_GROUP" "$dir" 2>/dev/null || \
        warn "could not hand $dir over to $PANGOLIN_USER:$PANGOLIN_GROUP"
    chmod 0750 "$dir" 2>/dev/null || true
    return 0
}

# Enable a unit for the next boot.
#
# `deb-systemd-helper` is the Debian-native way: it manipulates the
# symlink itself and works without a running systemd, which is what
# makes it usable from inside a container.  `systemctl enable` is the
# fallback for the RPM family.  The hand-made symlink is the last
# resort for a host that has neither — it is exactly what
# `systemctl enable` creates for a `WantedBy=multi-user.target` unit.
enable_unit() {
    unit="$1"
    src="$(unit_path "$unit")" || {
        warn "cannot enable $unit — the unit file is not installed"
        return 1
    }
    link="$root/etc/systemd/system/multi-user.target.wants/$unit"

    if [ -x "$root/usr/bin/deb-systemd-helper" ]; then
        "$root/usr/bin/deb-systemd-helper" unmask "$unit" >/dev/null 2>&1 || true
        "$root/usr/bin/deb-systemd-helper" enable "$unit" >/dev/null 2>&1 || true
    fi

    if [ ! -e "$link" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl enable "$unit" >/dev/null 2>&1 || true
    fi

    if [ ! -e "$link" ]; then
        mkdir -p "$(dirname "$link")" 2>/dev/null || true
        ln -sf "$src" "$link" 2>/dev/null || true
    fi

    if [ -e "$link" ]; then
        say "enabled $unit (it will not be started automatically)"
        return 0
    fi

    warn "could not enable $unit — enable it by hand with 'systemctl enable $unit'"
    return 1
}

# ── the state directories ───────────────────────────────────────────────────

if unit_path "$NGX_UNIT" >/dev/null 2>&1; then
    ensure_dir "$root/var/lib/pangolin"
    ensure_dir "$root/var/log/pangolin/bots"
    ensure_dir "$root/var/log/pangolin/referers"
    ensure_dir "$root/var/cache/pangolin/bots"
fi

if unit_path "$TUN_UNIT" >/dev/null 2>&1; then
    ensure_dir "$root/var/lib/pangolin-tun"
fi

# ── systemd ─────────────────────────────────────────────────────────────────

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || \
        warn "systemctl daemon-reload failed — systemd may not know the new unit"
else
    warn "systemctl was not found — the unit is installed but not registered"
fi

# Only the gateway is enabled.  The tunnel node stays disabled: it
# needs a `(name, token)` row in the gateway's admin UI before it can
# connect, and starting it without one is a restart loop.
if unit_path "$NGX_UNIT" >/dev/null 2>&1; then
    enable_unit "$NGX_UNIT"
fi

# ── an older installation on the same host ─────────────────────────────────

for legacy in "$root/usr/local/pangolin/pangolin-ngx" "$root/usr/local/pangolin/pangolin-tun"; do
    if [ -e "$legacy" ]; then
        warn "an older Pangolin installation was found at /usr/local/pangolin"
        warn "the package installs into /usr/bin, /etc/pangolin and /var/lib —"
        warn "the two layouts do not share any path, and nothing was migrated."
        warn "stop the old service before starting the packaged one:"
        warn "  systemctl stop pangolin-ngx   (or 'pangolin-tun')"
        warn "move the old data over by hand — see docs/packaging.md"
    fi
done

# ── what is left to do ──────────────────────────────────────────────────────

if unit_path "$NGX_UNIT" >/dev/null 2>&1; then
    say "edit /etc/pangolin/ngx.yml before the first start:"
    say "  - admin.password is still the public default 'admin'"
    say "  - acme.email is empty — put a real mailbox in"
    say "  - admin.addr is 0.0.0.0:9081 — narrow it down"
    say "then start the gateway:"
    say "  systemctl start pangolin-ngx"
fi

if unit_path "$TUN_UNIT" >/dev/null 2>&1; then
    say "edit /etc/pangolin/tun.yml before the first start:"
    say "  - token is empty — it must match a (name, token) row in the"
    say "    gateway's admin UI, or the handshake is rejected"
    say "  - server must point at the gateway's tunnel.addr"
    say "then start the tunnel node:"
    say "  systemctl start pangolin-tun"
fi

exit 0
