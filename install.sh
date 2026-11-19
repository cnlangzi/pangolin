#!/bin/sh
#
# One-line installer for the Pangolin release binaries.
#
#   curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh
#
# What it does, in order:
#
#   1. works out the platform (Linux / macOS, x86_64 / arm64) and the
#      version to install (the latest stable release unless
#      PANGOLIN_VERSION pins one);
#   2. downloads the release archive, the checksum file and the two
#      systemd units — and verifies every one of them against
#      SHA256SUMS.txt BEFORE touching the install directory, so a failed
#      install never leaves a half-written gateway behind;
#   3. unpacks the archive into the install directory, with the
#      binaries 0755 and the two configuration files 0600 (they carry
#      the admin password and the tun's bearer token);
#   4. rewrites the five gateway settings that have to change between a
#      developer checkout and a production host;
#   5. on Linux, creates the `pangolin` system user, installs the two
#      systemd units and starts the gateway;
#   6. prints what the operator still has to do.
#
# POSIX `sh` only.  macOS ships bash 3.2 and Linux bash 5.x, and a
# bashism would break the installer on one of the two platforms it
# exists to serve.
#
# Environment variables (all optional):
#
#   PANGOLIN_VERSION      version to install, e.g. v0.3.0.  Unset means
#                         "the latest stable release".
#   PANGOLIN_INSTALL_DIR  where the gateway is installed.
#                         Default: /usr/local/pangolin
#   PANGOLIN_BASE_URL     release download base.  The artefacts live at
#                         $PANGOLIN_BASE_URL/$VERSION/<asset>.
#                         Default: https://github.com/cnlangzi/pangolin/releases/download
#   PANGOLIN_LATEST_URL   where the latest version is resolved from.
#                         Default: https://github.com/cnlangzi/pangolin/releases/latest
#   PANGOLIN_NO_SERVICE   set to 1 to skip the system user, the systemd
#                         units and every systemctl call.
#   PANGOLIN_SYSTEMD_DIR  where the systemd units are installed.
#                         Default: /etc/systemd/system

set -eu

# ── configuration ───────────────────────────────────────────────────────────

install_dir_default='/usr/local/pangolin'
base_url_default='https://github.com/cnlangzi/pangolin/releases/download'
latest_url_default='https://github.com/cnlangzi/pangolin/releases/latest'
systemd_dir_default='/etc/systemd/system'

version="${PANGOLIN_VERSION:-}"
install_dir="${PANGOLIN_INSTALL_DIR:-$install_dir_default}"
base_url="${PANGOLIN_BASE_URL:-$base_url_default}"
latest_url="${PANGOLIN_LATEST_URL:-$latest_url_default}"
no_service="${PANGOLIN_NO_SERVICE:-}"
systemd_dir="${PANGOLIN_SYSTEMD_DIR:-$systemd_dir_default}"

# A trailing slash would leak into the systemd unit paths.
case "$install_dir" in
    /) ;;
    */) install_dir="${install_dir%/}" ;;
esac
case "$systemd_dir" in
    /) ;;
    */) systemd_dir="${systemd_dir%/}" ;;
esac

# The gateway's own defaults in the shipped ngx.yml.  The installer
# rewrites the two listen addresses and the log level; the tunnel port
# is left alone and only reported in the summary.
#
# The summary reports what is actually installed, so these are only the
# fallbacks for a configuration that does not carry the key.
admin_addr_default='0.0.0.0:9081'
admin_user_default='admin'
admin_password_default='pangolin'
tunnel_addr_default='0.0.0.0:9001'

# ── helpers ─────────────────────────────────────────────────────────────────

log() {
    printf '%s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "${work_dir:-}" ] && [ -d "${work_dir:-}" ]; then
        rm -rf "$work_dir"
    fi
}

# Escape the characters that are special in the replacement half of a
# `sed` expression: a backslash, an ampersand (which stands for the
# matched text) and the separator itself.
escape_sed() {
    printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}

# The admin UI ships wide open.  The warning goes out twice — before
# anything is downloaded and again in the closing summary — because it
# is the one thing the operator has to act on.
print_admin_warning() {
    cat <<'EOF'
--------------------------------------------------------------------------
WARNING: the admin UI ships wide open.
  * it listens on 0.0.0.0:9081 — every interface, not just localhost
  * it speaks plain HTTP, so the admin session crosses the wire in clear
  * the password the installer writes is the published value "pangolin"
    (the archive itself ships "admin") — both are public
Change the password and narrow `admin.addr` before this host is
reachable from an untrusted network.
--------------------------------------------------------------------------
EOF
}

# The value of one key of the gateway configuration, looked up by its
# section.
#
# The summary has to report the configuration that is actually
# installed — which is the operator's own file when they kept one — so
# the values are read back instead of being repeated here.
config_value() {
    awk -v want_section="$2" -v want_key="$3" '
        /^[A-Za-z_][A-Za-z0-9_]*:/ {
            section = $0
            sub(/:.*$/, "", section)
            next
        }
        section == want_section && $0 ~ ("^  " want_key ":") {
            value = $0
            sub(/^  [A-Za-z_][A-Za-z0-9_]*:[ \t]*/, "", value)
            sub(/[ \t]*#.*$/, "", value)
            sub(/[ \t]+$/, "", value)
            print value
            exit
        }
    ' "$1"
}

# The command that prints the whole install procedure — used when the
# script cannot do it itself (no systemd) and on macOS.
print_manual_start() {
    log ""
    log "Start the gateway by hand:"
    log "  cd $install_dir && ./pangolin-ngx --config $install_dir/ngx.yml"
    log ""
    log "The installed configuration binds $http_addr and $https_addr."
    if [ "$http_port" -lt 1024 ] 2>/dev/null; then
        log "Those are privileged ports: run the gateway as root, or grant it"
        log 'CAP_NET_BIND_SERVICE, or change `addr.http` / `addr.https` back to'
        log "the unprivileged 8000 / 8443."
    fi
}

# ── the platform ────────────────────────────────────────────────────────────

unsupported_platform() {
    die "unsupported platform: $os_name/$arch_name
Pangolin publishes one-line installers for these four platforms:
  linux_amd64   Linux   x86_64
  linux_arm64   Linux   aarch64
  macos_arm64   macOS   Apple Silicon
  macos_x64     macOS   Intel
Install by hand instead: https://github.com/cnlangzi/pangolin/blob/main/docs/getting-started.md"
}

os_name=$(uname -s 2>/dev/null || printf 'unknown')
arch_name=$(uname -m 2>/dev/null || printf 'unknown')

case "$os_name" in
    Linux)
        case "$arch_name" in
            x86_64|amd64) target='linux_amd64' ;;
            aarch64|arm64) target='linux_arm64' ;;
            *) unsupported_platform ;;
        esac
        ;;
    Darwin)
        case "$arch_name" in
            arm64|aarch64) target='macos_arm64' ;;
            x86_64|amd64) target='macos_x64' ;;
            *) unsupported_platform ;;
        esac
        ;;
    *)
        unsupported_platform
        ;;
esac

# ── preflight ───────────────────────────────────────────────────────────────

command -v curl >/dev/null 2>&1 || die "curl is required — install curl and run the command again"
command -v tar >/dev/null 2>&1 || die "tar is required — install tar and run the command again"

# The install directory ends up in the unit's `WorkingDirectory=` and
# `ExecStart=`.  systemd resolves a relative path against `/`, so a
# relative install directory would produce a unit that starts a binary
# that is not there.
case "$install_dir" in
    /*) ;;
    *) die "PANGOLIN_INSTALL_DIR must be an absolute path, got '$install_dir'" ;;
esac

print_admin_warning

# ── the version ─────────────────────────────────────────────────────────────

# A version is `v<something>`; the leading `v` is added when it is
# missing so that `PANGOLIN_VERSION=0.3.0` and `PANGOLIN_VERSION=v0.3.0`
# mean the same thing.
validate_version() {
    vv_tag="$1"
    vv_what="$2"
    case "$vv_tag" in
        '')
            die "could not determine the version to install ($vv_what)"
            ;;
        *[!A-Za-z0-9._+-]*)
            die "invalid version '$vv_tag' ($vv_what)"
            ;;
    esac
    case "$vv_tag" in
        v[0-9]*) ;;
        *) die "invalid version '$vv_tag' ($vv_what)" ;;
    esac
}

if [ -n "$version" ]; then
    case "$version" in
        v*) ;;
        *) version="v$version" ;;
    esac
    validate_version "$version" "PANGOLIN_VERSION"
else
    # The `releases/latest` URL redirects to the tag page of the newest
    # stable release; the tag is the last path segment of the URL the
    # redirect landed on.  No API call — an unauthenticated GitHub API
    # request is rate-limited to 60 per hour per address.
    latest_effective=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$latest_url") \
        || die "could not resolve the latest release from $latest_url"
    latest_effective=${latest_effective%%\?*}
    latest_effective=${latest_effective%%#*}
    latest_effective=${latest_effective%/}
    version=${latest_effective##*/}
    validate_version "$version" "the latest release at $latest_url"
fi

archive_name="pangolin_${version}_${target}.tar.gz"
archive_url="$base_url/$version/$archive_name"
sums_name='SHA256SUMS.txt'
sums_url="$base_url/$version/$sums_name"
unit_ngx_name='pangolin-ngx.service'
unit_tun_name='pangolin-tun.service'
unit_ngx_url="$base_url/$version/$unit_ngx_name"
unit_tun_url="$base_url/$version/$unit_tun_name"

# ── the privilege check ─────────────────────────────────────────────────────

# The steps that need root are: writing into the install directory,
# creating the system user, installing the systemd units and calling
# systemctl.  Everything else works as an unprivileged user.
is_root() {
    [ "$(id -u 2>/dev/null || printf '1')" = '0' ]
}

# The nearest existing ancestor of the install directory — the directory
# that has to be writable for the install to work at all.
install_dir_parent() {
    idp_dir="$1"
    while [ ! -e "$idp_dir" ]; do
        idp_up="${idp_dir%/*}"
        [ -n "$idp_up" ] || idp_up='/'
        if [ "$idp_up" = "$idp_dir" ]; then
            break
        fi
        idp_dir="$idp_up"
    done
    printf '%s' "$idp_dir"
}

# True when the systemd units are installed and the gateway is started.
service_is_planned() {
    [ "$os_name" = 'Linux' ] || return 1
    [ "$no_service" != '1' ] || return 1
    command -v systemctl >/dev/null 2>&1 || return 1
    return 0
}

if ! is_root; then
    if [ ! -w "$(install_dir_parent "$install_dir")" ]; then
        die "$install_dir is not writable by $(id -un 2>/dev/null || printf 'this user') — re-run with sudo:
  curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh"
    fi
    if service_is_planned; then
        die "installing the systemd units and starting the gateway needs root — re-run with sudo:
  curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh"
    fi
fi

# ── the downloads ───────────────────────────────────────────────────────────

work_dir=$(mktemp -d)
trap 'cleanup' 0
trap 'cleanup; exit 1' 1 2 15

fetch() {
    fetch_url="$1"
    fetch_dest="$2"
    fetch_code=$(curl -fsSL --retry 2 -o "$fetch_dest" -w '%{http_code}' "$fetch_url") || {
        rm -f "$fetch_dest"
        die "could not download $fetch_url (HTTP status: ${fetch_code:-unknown})"
    }
}

# Fetch a file that the release may not have at all.  A 404 is not an
# error here — the caller decides what a missing file means.
fetch_optional() {
    fo_url="$1"
    fo_dest="$2"
    fo_code=$(curl -fsSL -o "$fo_dest" -w '%{http_code}' "$fo_url") || {
        rm -f "$fo_dest"
        if [ "$fo_code" = '404' ]; then
            return 1
        fi
        die "could not download $fo_url (HTTP status: ${fo_code:-unknown})"
    }
    return 0
}

log "Downloading $archive_url"
fetch "$archive_url" "$work_dir/$archive_name"

log "Downloading $sums_url"
fetch "$sums_url" "$work_dir/$sums_name"

log "Downloading $unit_ngx_url"
if ! fetch_optional "$unit_ngx_url" "$work_dir/$unit_ngx_name"; then
    die "release $version does not ship $unit_ngx_name — it predates the one-line installer.
Only a release that ships the packaged configuration and the systemd units can be
installed with it.  Install this one by hand:
  https://github.com/cnlangzi/pangolin/blob/main/docs/getting-started.md"
fi

log "Downloading $unit_tun_url"
if ! fetch_optional "$unit_tun_url" "$work_dir/$unit_tun_name"; then
    die "release $version does not ship $unit_tun_name — it predates the one-line installer.
Only a release that ships the packaged configuration and the systemd units can be
installed with it.  Install this one by hand:
  https://github.com/cnlangzi/pangolin/blob/main/docs/getting-started.md"
fi

# ── the checksums ───────────────────────────────────────────────────────────

# The digest tool, in order of preference: sha256sum (GNU coreutils),
# shasum -a 256 (macOS, Perl) and openssl dgst -sha256 (the fallback on
# a host that has neither of the first two).
if command -v sha256sum >/dev/null 2>&1; then
    digest_of() {
        sha256sum "$1" | cut -d' ' -f1
    }
elif command -v shasum >/dev/null 2>&1; then
    digest_of() {
        shasum -a 256 "$1" | cut -d' ' -f1
    }
elif command -v openssl >/dev/null 2>&1; then
    digest_of() {
        openssl dgst -sha256 "$1" | sed -e 's/^.*= *//'
    }
else
    die "no SHA-256 tool found — install one of sha256sum, shasum or openssl and run the command again"
fi

# The recorded digest of one file.  SHA256SUMS.txt lists bare file
# names, one per line, in `sha256sum` format — but it also lists the
# five archives this machine did not download, so the entry is looked up
# instead of running `sha256sum -c` over the whole file.
recorded_digest() {
    awk -v want="$2" '
        {
            name = $0
            sub(/^[0-9a-fA-F]+[[:space:]]+\*?/, "", name)
            if (name == want) {
                print $1
                exit
            }
        }
    ' "$1"
}

verify_digest() {
    vd_file="$1"
    vd_name="$2"
    vd_want=$(recorded_digest "$work_dir/$sums_name" "$vd_name")
    if [ -z "$vd_want" ]; then
        die "the $sums_name of release $version has no entry for $vd_name — refusing to install"
    fi
    vd_got=$(digest_of "$vd_file")
    if [ "$(printf '%s' "$vd_want" | tr 'A-Z' 'a-z')" != "$(printf '%s' "$vd_got" | tr 'A-Z' 'a-z')" ]; then
        die "checksum mismatch for $vd_name
  expected: $vd_want
  got:      $vd_got
Nothing was installed — the download is corrupt or the mirror is wrong."
    fi
    log "  ok  $vd_name"
}

log "Verifying the downloads against $sums_name"
verify_digest "$work_dir/$archive_name" "$archive_name"
verify_digest "$work_dir/$unit_ngx_name" "$unit_ngx_name"
verify_digest "$work_dir/$unit_tun_name" "$unit_tun_name"

# ── the system user ─────────────────────────────────────────────────────────

# The gateway and the tunnel node run as an unprivileged user; the unit
# files say so.  The user is created before the files are installed,
# because the install directory is handed over to it right after.
pangolin_user_exists() {
    id -u pangolin >/dev/null 2>&1
}

pangolin_group_exists() {
    getent group pangolin >/dev/null 2>&1
}

ensure_pangolin_user() {
    if pangolin_user_exists; then
        log "The system user 'pangolin' already exists — keeping it."
        return 0
    fi
    if command -v useradd >/dev/null 2>&1; then
        # The group has to exist as well: the unit says `Group=pangolin`
        # and the install directory is handed over to `pangolin:pangolin`.
        # `useradd` only creates a same-named group where
        # USERGROUPS_ENAB is yes (Debian) — on the RHEL family the user
        # would land in the default group instead, and the chown below
        # would fail.  `--user-group` asks for the group explicitly.
        if pangolin_group_exists; then
            useradd --system --no-create-home --shell /usr/sbin/nologin pangolin
        else
            useradd --system --no-create-home --user-group --shell /usr/sbin/nologin pangolin
        fi
    elif command -v adduser >/dev/null 2>&1; then
        # Debian's adduser creates the group with the user.
        adduser --system pangolin
    else
        die "neither useradd nor adduser is available — cannot create the 'pangolin' system user"
    fi
    log "Created the system user 'pangolin'."
}

if service_is_planned; then
    ensure_pangolin_user
fi

# ── unpacking ───────────────────────────────────────────────────────────────

stage_dir="$work_dir/stage"
mkdir -p "$stage_dir"
tar -xzf "$work_dir/$archive_name" -C "$stage_dir"

# The archive of a release that predates the packaged configuration
# holds the two binaries and nothing else.  There is no configuration
# to install and nothing to rewrite, so such a release cannot be
# installed by this script.
if [ ! -f "$stage_dir/ngx.yml" ] || [ ! -f "$stage_dir/tun.yml" ]; then
    die "release $version ships no packaged configuration (ngx.yml / tun.yml) — it
predates the one-line installer.  Install this one by hand:
  https://github.com/cnlangzi/pangolin/blob/main/docs/getting-started.md"
fi

# One file at a time, because the operator's own configuration must
# survive a re-run: the shipped ngx.yml / tun.yml are only installed
# when they are not there yet, and an existing binary is moved aside to
# <name>.bak first — the same rolling-backup convention the Ansible
# role uses.
install_binary() {
    ib_src="$1"
    ib_dst="$2"
    ib_name="${ib_dst##*/}"
    if [ -e "$ib_dst" ]; then
        mv "$ib_dst" "$ib_dst.bak"
        log "  kept the previous $ib_name as $ib_name.bak"
    fi
    cp "$ib_src" "$ib_dst"
    chmod 0755 "$ib_dst"
}

install_config() {
    ic_src="$1"
    ic_dst="$2"
    ic_name="${ic_dst##*/}"
    if [ -e "$ic_dst" ]; then
        log "  kept the existing $ic_name — it was not overwritten"
        return 0
    fi
    cp "$ic_src" "$ic_dst"
    chmod 0600 "$ic_dst"
}

log "Installing into $install_dir"
mkdir -p "$install_dir"

install_binary "$stage_dir/pangolin-ngx" "$install_dir/pangolin-ngx"
install_binary "$stage_dir/pangolin-tun" "$install_dir/pangolin-tun"

# The flag is read before the call: `install_config` keeps an existing
# file, and only a freshly installed one may be rewritten.
ngx_installed=0
if [ ! -e "$install_dir/ngx.yml" ]; then
    ngx_installed=1
fi
install_config "$stage_dir/ngx.yml" "$install_dir/ngx.yml"
install_config "$stage_dir/tun.yml" "$install_dir/tun.yml"

# The relative paths in the shipped configuration (./certs, ./logs/bots,
# ./logs/referers) resolve against the unit's working directory, so the
# directories have to exist before the gateway starts.
mkdir -p "$install_dir/certs" "$install_dir/logs/bots" "$install_dir/logs/referers"

# ── the configuration rewrite ───────────────────────────────────────────────

# The ACME contact address is derived from the host name: lower-cased,
# with everything that cannot appear in a host name replaced by a dash.
# An empty result falls back to "localhost".
acme_email_host=$(hostname -s 2>/dev/null || printf '')
acme_email_host=$(printf '%s' "$acme_email_host" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9-' '-')
acme_email_host=$(printf '%s' "$acme_email_host" | sed -e 's/^-*//' -e 's/-*$//')
[ -n "$acme_email_host" ] || acme_email_host='localhost'
acme_email="pangolin@${acme_email_host}.local"

# Five keys change between a developer checkout and a production host:
# the two listen addresses, the admin password, the ACME contact and the
# log level.  Everything else — including `admin.addr`, which stays at
# 0.0.0.0:9081 — is left exactly as the release shipped it.
#
# The keys are located by their section, not by their name alone: `addr`
# is a section of its own and a key under `admin` and under `tunnel`,
# and a global substitution would hit all three.
rewrite_ngx_config() {
    awk \
        -v addr_http='0.0.0.0:80' \
        -v addr_https='0.0.0.0:443' \
        -v admin_password="$admin_password_default" \
        -v acme_email="\"$acme_email\"" \
        -v log_level='info' '
        {
            line = $0

            # A top-level key opens a section.  The keys below it
            # belong to it until the next top-level key.
            if (line ~ /^[A-Za-z_][A-Za-z0-9_]*:/) {
                section = line
                sub(/:.*$/, "", section)
                print line
                next
            }

            if (line ~ /^  [A-Za-z_][A-Za-z0-9_]*:/) {
                key = line
                sub(/^  /, "", key)
                sub(/:.*$/, "", key)

                value = ""
                if (section == "addr" && key == "http") value = addr_http
                else if (section == "addr" && key == "https") value = addr_https
                else if (section == "admin" && key == "password") value = admin_password
                else if (section == "acme" && key == "email") value = acme_email
                else if (section == "log" && key == "level") value = log_level

                if (value != "") {
                    if (match(line, /^  [A-Za-z_][A-Za-z0-9_]*:[ \t]*[^ \t]+/)) {
                        head = substr(line, 1, RLENGTH)
                        tail = substr(line, RLENGTH + 1)
                        # Replace the value only — the key and the
                        # trailing comment stay as they were.
                        sub(/[^ \t]+$/, value, head)
                        print head tail
                    } else {
                        sub(/:[ \t]*$/, ": " value, line)
                        print line
                    }
                    next
                }
            }

            print line
        }
    ' "$1" > "$2"
}

if [ "$ngx_installed" -eq 1 ]; then
    rewrite_ngx_config "$install_dir/ngx.yml" "$work_dir/ngx.yml"
    # The rewritten file carries the admin password — it is 0600
    # before it is moved into place, never 0644.
    chmod 0600 "$work_dir/ngx.yml"
    mv "$work_dir/ngx.yml" "$install_dir/ngx.yml"
    log "  rewrote addr.http/addr.https, admin.password, acme.email and log.level in ngx.yml"
else
    log "  ngx.yml was not rewritten — the existing configuration was kept"
fi

# ── the installed configuration ─────────────────────────────────────────────

# The summary and the manual start instructions report the configuration
# that is actually installed — which is the operator's own file when
# they kept one.
admin_addr=$(config_value "$install_dir/ngx.yml" admin addr)
admin_user=$(config_value "$install_dir/ngx.yml" admin username)
admin_password=$(config_value "$install_dir/ngx.yml" admin password)
http_addr=$(config_value "$install_dir/ngx.yml" addr http)
https_addr=$(config_value "$install_dir/ngx.yml" addr https)
tunnel_addr=$(config_value "$install_dir/ngx.yml" tunnel addr)

[ -n "$admin_addr" ] || admin_addr="$admin_addr_default"
[ -n "$admin_user" ] || admin_user="$admin_user_default"
[ -n "$admin_password" ] || admin_password="$admin_password_default"
[ -n "$http_addr" ] || http_addr='0.0.0.0:80'
[ -n "$https_addr" ] || https_addr='0.0.0.0:443'
[ -n "$tunnel_addr" ] || tunnel_addr="$tunnel_addr_default"

admin_port="${admin_addr##*:}"
http_port="${http_addr##*:}"
https_port="${https_addr##*:}"
tunnel_port="${tunnel_addr##*:}"

# ── the ownership ───────────────────────────────────────────────────────────

# The gateway writes its SQLite database, its certificates and its logs
# into the install directory, so the directory belongs to the user the
# unit runs as.
if pangolin_user_exists; then
    chown -R "pangolin:pangolin" "$install_dir" \
        || die "could not hand $install_dir over to the 'pangolin' user — does the group 'pangolin' exist?"
    log "  $install_dir now belongs to pangolin:pangolin"
fi

# ── the service ─────────────────────────────────────────────────────────────

install_unit() {
    iu_src="$1"
    iu_name="$2"
    iu_dst="$systemd_dir/$iu_name"

    # The unit ships with the default install directory baked in.  A
    # different install directory has to be reflected in the unit, or
    # systemd would start a binary that is not there.
    mkdir -p "$systemd_dir"
    sed -e "s|$(escape_sed "$install_dir_default")|$(escape_sed "$install_dir")|g" \
        "$iu_src" > "$iu_dst.new"
    chmod 0644 "$iu_dst.new"
    # A unit the operator edited is kept as <name>.bak, the same
    # rolling-backup convention the binaries get.
    if [ -e "$iu_dst" ]; then
        mv "$iu_dst" "$iu_dst.bak"
        log "  kept the previous $iu_name as $iu_name.bak"
    fi
    mv "$iu_dst.new" "$iu_dst"
}

# The service is the last thing the script does, and a service that
# does not come up must not swallow the summary — the operator needs
# the credentials, the ports and the tunnel steps exactly then.
service_ok=1

if service_is_planned; then
    log "Installing the systemd units into $systemd_dir"
    install_unit "$work_dir/$unit_ngx_name" "$unit_ngx_name"
    install_unit "$work_dir/$unit_tun_name" "$unit_tun_name"

    if ! systemctl daemon-reload; then
        warn "systemctl daemon-reload failed — systemd may not know the new unit"
        service_ok=0
    fi

    # `enable --now` starts the unit — but a unit that is already
    # running is left alone, and then the binary that was just
    # installed would not be the one serving.  Restart it in that case,
    # the way the Ansible role's restart handler does.
    if systemctl is-active --quiet pangolin-ngx; then
        was_active=1
    else
        was_active=0
    fi

    # The tunnel node is installed but NOT started: the gateway rejects
    # a tun whose (name, token) row does not exist yet, so starting it
    # now would only produce a restart loop.
    if ! systemctl enable --now pangolin-ngx; then
        warn "systemctl could not start the gateway — check 'systemctl status pangolin-ngx'"
        service_ok=0
    elif [ "$was_active" -eq 1 ]; then
        log "  the gateway was already running — restarting it with the new binary"
        if ! systemctl restart pangolin-ngx; then
            warn "systemctl could not restart the gateway — check 'systemctl status pangolin-ngx'"
            service_ok=0
        fi
    else
        # The start job is only enqueued at this point, so the unit may
        # not be up yet.  Give it a moment before the summary claims it
        # is running.
        waited=0
        while [ "$waited" -lt 5 ]; do
            if systemctl is-active --quiet pangolin-ngx; then
                break
            fi
            waited=$((waited + 1))
            sleep 1
        done
        if [ "$waited" -ge 5 ]; then
            warn "the gateway did not come up — check 'systemctl status pangolin-ngx'"
            service_ok=0
        fi
    fi
    log "  systemctl enable --now pangolin-ngx"
else
    if [ "$no_service" = '1' ]; then
        log "PANGOLIN_NO_SERVICE=1 — no system user, no systemd unit, no systemctl call."
    elif [ "$os_name" != 'Linux' ]; then
        log "macOS has no systemd — the gateway is not registered as a service."
    else
        warn "systemctl was not found — the gateway was not registered as a service."
    fi
    print_manual_start
fi

# ── the summary ─────────────────────────────────────────────────────────────

host_name=$(hostname -s 2>/dev/null || printf 'this-host')

log ""
log "--------------------------------------------------------------------------"
if [ "$service_ok" -eq 1 ]; then
    log "Pangolin $version installed."
else
    log "Pangolin $version installed — but the gateway is not running."
    log "Check 'systemctl status pangolin-ngx' and 'journalctl -u pangolin-ngx'."
fi
log ""
log "  install directory : $install_dir"
log "  version           : $version"
log "  admin UI          : http://$host_name:$admin_port/  (bound to $admin_addr)"
log "  admin credentials : $admin_user / $admin_password"
log "  database          : $install_dir/pangolin.db"
log ""
log "  Ports to open:"
log "    TCP $http_port            HTTP"
log "    TCP $https_port           HTTPS"
log "    TCP+UDP $tunnel_port      tunnel (QUIC on UDP, WebSocket/TCP fallback)"
log ""
log "  The tunnel node is NOT running yet.  To register one:"
log "    1. create its (name, token) row in the admin UI (Tunnels page) —"
log "       the gateway does not register tunnel nodes on its own"
log "    2. put the same values into $install_dir/tun.yml (server, token, name)"
if service_is_planned; then
    log "    3. systemctl enable --now pangolin-tun"
else
    log "    3. start pangolin-tun by hand"
fi
log ""
print_admin_warning

# A gateway that did not come up is a failed install: the files are in
# place, but the operator asked for a running gateway.
if [ "$service_ok" -ne 1 ]; then
    exit 1
fi
