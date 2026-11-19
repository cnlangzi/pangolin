#!/bin/sh
# preinstall — create the `pangolin` system user and group.
#
# Runs before the package's own files are laid down.  The user has to
# exist first, because the package's directories are handed over to it
# and its unit file says `User=pangolin` / `Group=pangolin`.
#
# The script is deliberately package-agnostic: both `pangolin-ngx` and
# `pangolin-tun` run as the same user, and either package may be
# installed on its own.
#
# Idempotent by construction — an existing `pangolin` user or group is
# kept as it is.  The operator may have created it themselves (the
# Ansible role and `install.sh` create it too), and re-installing the
# package must not fail because of that.
#
# POSIX `sh`: dpkg runs maintainer scripts with `/bin/sh`, and so does
# rpm's scriptlet interpreter.

set -e

PANGOLIN_USER=pangolin
PANGOLIN_GROUP=pangolin
PANGOLIN_HOME=/var/lib/pangolin
PANGOLIN_SHELL=/usr/sbin/nologin

group_exists() {
    getent group "$1" >/dev/null 2>&1
}

user_exists() {
    id -u "$1" >/dev/null 2>&1
}

if ! group_exists "$PANGOLIN_GROUP"; then
    if command -v groupadd >/dev/null 2>&1; then
        groupadd --system "$PANGOLIN_GROUP"
    elif command -v addgroup >/dev/null 2>&1; then
        addgroup --system "$PANGOLIN_GROUP"
    else
        echo "pangolin: neither groupadd nor addgroup is available — cannot create" \
             "the '$PANGOLIN_GROUP' group" >&2
        exit 1
    fi
fi

if ! user_exists "$PANGOLIN_USER"; then
    if command -v useradd >/dev/null 2>&1; then
        # `--gid` names the primary group explicitly.  It has to:
        # `useradd` only creates a same-named group where
        # USERGROUPS_ENAB is yes (Debian) — on the RHEL family the
        # user would land in the default group instead, and the
        # unit's `Group=pangolin` would then name a group the user
        # is not in.
        #
        # `--user-group` is *not* an option here: it asks useradd to
        # create the group, and useradd refuses when a group of that
        # name already exists — which it does, two lines above (and
        # on a host where the Ansible role or `install.sh` created
        # it first).  The install would abort on every machine.
        useradd --system --no-create-home --gid "$PANGOLIN_GROUP" \
            --home-dir "$PANGOLIN_HOME" --shell "$PANGOLIN_SHELL" \
            "$PANGOLIN_USER"
    elif command -v adduser >/dev/null 2>&1; then
        # Debian's adduser creates the group with the user.
        adduser --system --home "$PANGOLIN_HOME" --no-create-home \
            --shell "$PANGOLIN_SHELL" "$PANGOLIN_USER"
    else
        echo "pangolin: neither useradd nor adduser is available — cannot create" \
             "the '$PANGOLIN_USER' system user" >&2
        exit 1
    fi
fi

exit 0
