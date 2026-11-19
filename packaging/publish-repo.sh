#!/bin/sh
# publish-repo.sh — build the apt and rpm repositories out of the
# packages and sign them.
#
# The result is the *site tree* — the directory GitHub Pages serves:
#
#   .nojekyll
#   index.html
#   pangolin-archive-keyring.asc
#   pangolin-archive-keyring.gpg
#   dists/stable/Release
#   dists/stable/Release.gpg
#   dists/stable/InRelease
#   dists/stable/main/binary-<arch>/Packages
#   dists/stable/main/binary-<arch>/Packages.gz
#   pool/main/p/<package>/…
#   rpm/…
#   rpm/repodata/…
#
# Usage:
#
#   publish-repo.sh --site <dir> --deb-dir <dir> --rpm-dir <dir> \
#                   --version <ver> [--sign]
#
# The architectures are derived from the `.deb` file names, so the
# caller does not have to keep a list in sync with what it built.
#
# Signing is opt-in (`--sign`) and needs a secret key in the current
# GnuPG home.  The passphrase — when the key has one — is read from
# `PANGOLIN_SIGNING_PASSPHRASE`; it is never written to a file the
# job could leak and never passed on a command line.
#
# `createrepo_c` cannot sign: it has no `--sign` option, so the
# repository signature (`repodata/repomd.xml.asc`) is produced with
# `gpg --detach-sign` right after the metadata is generated — the
# same file `createrepo_c --sign` would have written.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
packaging_dir="$script_dir"

site=""
deb_dir=""
rpm_dir=""
version=""
do_sign=0

usage() {
    echo "usage: publish-repo.sh --site <dir> --deb-dir <dir> --rpm-dir <dir>" >&2
    echo "                          --version <ver> [--sign]" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --site)
            site="$2"
            shift 2
            ;;
        --deb-dir)
            deb_dir="$2"
            shift 2
            ;;
        --rpm-dir)
            rpm_dir="$2"
            shift 2
            ;;
        --version)
            version="$2"
            shift 2
            ;;
        --sign)
            do_sign=1
            shift
            ;;
        -h | --help)
            usage
            ;;
        *)
            echo "publish-repo.sh: unknown argument '$1'" >&2
            usage
            ;;
    esac
done

for required in site deb_dir rpm_dir version; do
    eval "value=\$$required"
    if [ -z "$value" ]; then
        echo "publish-repo.sh: missing --$required" >&2
        usage
    fi
done

mkdir -p "$site"
site=$(CDPATH= cd -- "$site" && pwd)

suite=stable
component=main

# ── the signing material ────────────────────────────────────────────────────

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

passphrase_file=""
gpg_extra=""

if [ "$do_sign" -eq 1 ]; then
    if ! command -v gpg >/dev/null 2>&1; then
        echo "publish-repo.sh: gpg is not on PATH — cannot sign the repository" >&2
        exit 1
    fi
    if [ -n "${PANGOLIN_SIGNING_PASSPHRASE:-}" ]; then
        passphrase_file="$work/passphrase"
        printf '%s' "$PANGOLIN_SIGNING_PASSPHRASE" > "$passphrase_file"
        chmod 600 "$passphrase_file"
        # gpg reads the passphrase from the file instead of asking for
        # it — a job has no terminal to ask on.
        gpg_extra="--batch --yes --pinentry-mode loopback --passphrase-file $passphrase_file"
    else
        gpg_extra="--batch --yes"
    fi
fi

# The key the signatures are made with.  `PANGOLIN_SIGNING_KEY` names
# it explicitly; without it the only secret key in the keyring is
# used, which is what a single-key archive keyring needs.
signing_key="${PANGOLIN_SIGNING_KEY:-}"
if [ -z "$signing_key" ] && [ "$do_sign" -eq 1 ]; then
    signing_key=$(gpg --list-secret-keys --with-colons | awk -F: '/^sec/ {print $5; exit}')
    if [ -z "$signing_key" ]; then
        echo "publish-repo.sh: no secret key in the keyring — cannot sign the repository" >&2
        exit 1
    fi
fi

gpg_run() {
    if [ -n "$signing_key" ]; then
        gpg $gpg_extra --local-user "$signing_key" "$@"
    else
        gpg $gpg_extra "$@"
    fi
}

# ── the apt repository ──────────────────────────────────────────────────────

pool="$site/pool/$component"
mkdir -p "$pool"

architectures=""

for deb in "$deb_dir"/*.deb; do
    [ -e "$deb" ] || continue
    name=$(basename "$deb")
    # pangolin-ngx_0.3.0-1_amd64.deb → pangolin-ngx / amd64
    package=$(printf '%s' "$name" | sed -e 's/_.*//')
    arch=$(printf '%s' "$name" | sed -e 's/.*_//' -e 's/\.deb$//')
    # The pool path follows Debian's convention: the component, then
    # the first letter of the package name, then the package itself.
    initial=$(printf '%s' "$package" | cut -c1)
    mkdir -p "$pool/$initial/$package"
    cp "$deb" "$pool/$initial/$package/$name"
    case " $architectures " in
        *" $arch "*) ;;
        *) architectures="$architectures $arch" ;;
    esac
done

if [ -z "$architectures" ]; then
    echo "publish-repo.sh: no .deb files in $deb_dir" >&2
    exit 1
fi

dists="$site/dists/$suite"
mkdir -p "$dists"

# The `Packages` index, one per architecture.
#
# `dpkg-scanpackages` is run from the site root with a *relative*
# pool path, so the `Filename:` field it writes is relative to the
# site root — which is what apt resolves against the repository's
# base URL.
for arch in $architectures; do
    arch_dir="$dists/$component/binary-$arch"
    mkdir -p "$arch_dir"
    (cd "$site" && dpkg-scanpackages --arch "$arch" --multiversion pool /dev/null) \
        > "$arch_dir/Packages"
    # `-n` omits the original name and the timestamp from the gzip
    # header: the same packages have to produce the same bytes on
    # every run, or the checksums in `Release` would not match.
    gzip -9 -n -c "$arch_dir/Packages" > "$arch_dir/Packages.gz"
done

# The `Release` file: the index of the index.
#
# No `Valid-Until`: the repository is refreshed when a release
# happens, not on a timer, and an expiry date would turn "no release
# for three months" into "apt update fails on every machine".
release="$dists/Release"
{
    echo "Origin: Pangolin"
    echo "Label: Pangolin"
    echo "Suite: $suite"
    echo "Codename: $suite"
    echo "Version: $version"
    # `$architectures` already carries the leading space.
    echo "Architectures:$architectures"
    echo "Components: $component"
    echo "Date: $(date -u '+%a, %d %b %Y %H:%M:%S +0000')"
    echo "SHA256:"
    for arch in $architectures; do
        for file in "$component/binary-$arch/Packages" "$component/binary-$arch/Packages.gz"; do
            path="$dists/$file"
            sum=$(sha256sum "$path" | cut -d' ' -f1)
            size=$(wc -c < "$path" | tr -d ' ')
            echo " $sum $size $file"
        done
    done
} > "$release"

if [ "$do_sign" -eq 1 ]; then
    # Two signatures over the same text: `InRelease` is the
    # clearsigned form apt prefers, `Release.gpg` the detached form
    # older clients need.  Both carry the identical `Release` text.
    gpg_run --clearsign --output "$dists/InRelease" "$release"
    gpg_run --detach-sign --output "$dists/Release.gpg" "$release"
fi

# ── the rpm repository ──────────────────────────────────────────────────────

rpm_root="$site/rpm"
mkdir -p "$rpm_root"

for rpm in "$rpm_dir"/*.rpm; do
    [ -e "$rpm" ] || continue
    cp "$rpm" "$rpm_root/$(basename "$rpm")"
done

if [ "$do_sign" -eq 1 ]; then
    # The packages are signed *before* the metadata is generated: the
    # metadata records the hash of the package, and a hash computed
    # before the signature was added does not match the file that
    # ends up in the repository — `gpgcheck=1` would then fail.
    if ! command -v rpm >/dev/null 2>&1; then
        echo "publish-repo.sh: rpm is not on PATH — cannot sign the packages" >&2
        exit 1
    fi

    # The key the signatures are made with.
    key_id="$signing_key"
    if [ -z "$key_id" ]; then
        key_id=$(gpg --list-secret-keys --with-colons | awk -F: '/^sec/ {print $5; exit}')
    fi
    if [ -z "$key_id" ]; then
        echo "publish-repo.sh: no secret key in the keyring — cannot sign the packages" >&2
        exit 1
    fi
    echo "signing the packages with $key_id"
    # rpm's gpg invocation is configured through three macros:
    #
    #   `_gpg_path` — rpm *overrides* `GNUPGHOME` with it, so the
    #     archive keyring has to be named here or rpm would look for
    #     the key in the invoking user's own `~/.gnupg`;
    #   `_gpg_name` — the key;
    #   `_gpg_sign_cmd_extra_args` — appended to rpm's own gpg
    #     command line.  `--batch` keeps gpg from asking on a
    #     terminal, and the loopback passphrase hands it the
    #     passphrase of a protected key without a prompt.
    gpg_path="${GNUPGHOME:-$HOME/.gnupg}"
    if [ -n "$passphrase_file" ]; then
        sign_extra="--batch --no-tty --pinentry-mode loopback --passphrase-file $passphrase_file"
    else
        sign_extra="--batch --no-tty"
    fi

    for rpm in "$rpm_root"/*.rpm; do
        [ -e "$rpm" ] || continue
        rpm --addsign \
            --define "_gpg_name $key_id" \
            --define "_gpg_path $gpg_path" \
            --define "_gpg_sign_cmd_extra_args $sign_extra" \
            "$rpm"
    done
fi

if command -v createrepo_c >/dev/null 2>&1; then
    createrepo_c "$rpm_root"
else
    echo "publish-repo.sh: createrepo_c is not on PATH — the rpm metadata was not generated" >&2
    exit 1
fi

if [ "$do_sign" -eq 1 ]; then
    # The repository signature.  `createrepo_c` has no `--sign`, so
    # the detached signature of `repomd.xml` is made here — it is the
    # file `repo_gpgcheck=1` looks for.
    gpg_run --detach-sign --armor \
        --output "$rpm_root/repodata/repomd.xml.asc" \
        "$rpm_root/repodata/repomd.xml"
fi

# ── the public key ──────────────────────────────────────────────────────────

key_src="$packaging_dir/keys/pangolin-archive-keyring.asc"
if [ -f "$key_src" ]; then
    cp "$key_src" "$site/pangolin-archive-keyring.asc"
    # The dearmored form is what apt's `signed-by=` wants: it is a
    # keyring, not an armored block.
    if command -v gpg >/dev/null 2>&1; then
        gpg --dearmor --yes --output "$site/pangolin-archive-keyring.gpg" "$key_src"
    fi
fi

# ── the site's front door ───────────────────────────────────────────────────

# GitHub Pages runs every Jekyll pass over the tree; `.nojekyll` turns
# that off, so a file that starts with an underscore is served as it
# is instead of being swallowed by the Jekyll build.
: > "$site/.nojekyll"

if [ ! -f "$site/index.html" ]; then
    cat > "$site/index.html" <<'HTML'
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Pangolin package repository</title>
</head>
<body>
<h1>Pangolin package repository</h1>
<p>
This is a package repository, not a website.  Add it to your package
manager:
</p>
<pre><code># Debian / Ubuntu
deb [signed-by=/etc/apt/keyrings/pangolin-archive-keyring.gpg] https://cnlangzi.github.io/pangolin stable main
</code></pre>
<pre><code># RHEL / Rocky / Alma
[pangolin]
name=Pangolin
baseurl=https://cnlangzi.github.io/pangolin/rpm/
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.asc
</code></pre>
<p>
The documentation is at
<a href="https://github.com/cnlangzi/pangolin/blob/main/docs/packaging.md">docs/packaging.md</a>.
</p>
</body>
</html>
HTML
fi

echo "published $site"
find "$site" -type f | LC_ALL=C sort | sed -e "s|^$site/||"
