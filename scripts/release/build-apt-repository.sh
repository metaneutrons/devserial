#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Fabian Schmieder
#
# Build a signed APT repository from the Debian packages of one release.
#
# The result is a plain directory tree that can be copied to any static host:
#
#   pool/main/d/devserial/devserial_1.2.3_amd64.deb
#   dists/stable/main/binary-amd64/Packages{,.gz}
#   dists/stable/Release            unsigned index
#   dists/stable/InRelease          index with an inline signature
#   dists/stable/Release.gpg        detached signature, for older clients
#   devserial-archive-keyring.gpg   public key, for signed-by=
#
# Upload the pool before the metadata. Metadata that mentions a package which
# is not there yet breaks every client that refreshes in between.
#
# Usage:
#   build-apt-repository.sh --deb-dir dist --output-dir repository \
#       --private-key key.asc --passphrase-file pass.txt --fingerprint FPR

set -euo pipefail

ORIGIN=${APT_ORIGIN:-devserial}
LABEL=${APT_LABEL:-devserial}
SUITE=${APT_SUITE:-stable}
CODENAME=${APT_CODENAME:-stable}
COMPONENT=main
ARCHITECTURES=${APT_ARCHITECTURES:-amd64 arm64}
DESCRIPTION=${APT_DESCRIPTION:-'devserial release packages'}
KEYRING_NAME=devserial-archive-keyring.gpg

deb_dir=''
output_dir=''
private_key=''
passphrase_file=''
fingerprint=''

die() {
  echo "build-apt-repository: $*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --deb-dir) deb_dir=$2; shift 2 ;;
    --output-dir) output_dir=$2; shift 2 ;;
    --private-key) private_key=$2; shift 2 ;;
    --passphrase-file) passphrase_file=$2; shift 2 ;;
    --fingerprint) fingerprint=$2; shift 2 ;;
    *) die "unknown argument '$1'" ;;
  esac
done

[[ -n "$deb_dir" ]] || die 'missing --deb-dir'
[[ -n "$output_dir" ]] || die 'missing --output-dir'
[[ -n "$private_key" ]] || die 'missing --private-key'
[[ -n "$fingerprint" ]] || die 'missing --fingerprint'
[[ -d "$deb_dir" ]] || die "'$deb_dir' does not exist"
[[ -f "$private_key" ]] || die "'$private_key' does not exist"

for tool in dpkg-scanpackages apt-ftparchive gpg; do
  command -v "$tool" >/dev/null 2>&1 || die "'$tool' is not installed"
done

rm -rf "$output_dir"
pool="${output_dir}/pool/${COMPONENT}/d/devserial"
mkdir -p "$pool"

shopt -s nullglob
debs=("$deb_dir"/*.deb)
shopt -u nullglob
[[ ${#debs[@]} -gt 0 ]] || die "no .deb files in '$deb_dir'"
cp "${debs[@]}" "$pool/"

# ------------------------------------------------------------------- indices

for arch in $ARCHITECTURES; do
  target="${output_dir}/dists/${SUITE}/${COMPONENT}/binary-${arch}"
  mkdir -p "$target"
  # Paths inside Packages are relative to the repository root, so the scan runs
  # from there.
  (cd "$output_dir" && dpkg-scanpackages --arch "$arch" pool /dev/null) \
    > "${target}/Packages" 2>/dev/null
  gzip -9 -c "${target}/Packages" > "${target}/Packages.gz"
  echo "  ${arch}: $(grep -c '^Package:' "${target}/Packages") package(s)"
done

release_dir="${output_dir}/dists/${SUITE}"
(
  cd "$output_dir"
  apt-ftparchive \
    -o "APT::FTPArchive::Release::Origin=${ORIGIN}" \
    -o "APT::FTPArchive::Release::Label=${LABEL}" \
    -o "APT::FTPArchive::Release::Suite=${SUITE}" \
    -o "APT::FTPArchive::Release::Codename=${CODENAME}" \
    -o "APT::FTPArchive::Release::Components=${COMPONENT}" \
    -o "APT::FTPArchive::Release::Architectures=${ARCHITECTURES}" \
    -o "APT::FTPArchive::Release::Description=${DESCRIPTION}" \
    release "dists/${SUITE}"
) > "${release_dir}/Release"

# ------------------------------------------------------------------ signing

export GNUPGHOME
GNUPGHOME=$(mktemp -d)
chmod 700 "$GNUPGHOME"
trap 'rm -rf "$GNUPGHOME"' EXIT

gpg --batch --quiet --import "$private_key"

gpg_sign() {
  if [[ -n "$passphrase_file" ]]; then
    gpg --batch --yes --pinentry-mode loopback --passphrase-file "$passphrase_file" \
        --local-user "$fingerprint" "$@"
  else
    gpg --batch --yes --pinentry-mode loopback --passphrase '' \
        --local-user "$fingerprint" "$@"
  fi
}

gpg_sign --clearsign --output "${release_dir}/InRelease" "${release_dir}/Release"
gpg_sign --detach-sign --armor --output "${release_dir}/Release.gpg" "${release_dir}/Release"

# The public key in the binary form that `signed-by=` expects.
gpg --batch --export "$fingerprint" > "${output_dir}/${KEYRING_NAME}"

# ------------------------------------------------------------ self-checking

gpg --batch --verify "${release_dir}/InRelease" >/dev/null 2>&1 \
  || die 'the generated InRelease does not verify'
gpg --batch --verify "${release_dir}/Release.gpg" "${release_dir}/Release" >/dev/null 2>&1 \
  || die 'the generated Release.gpg does not verify'
[[ -s "${output_dir}/${KEYRING_NAME}" ]] || die 'the exported keyring is empty'

echo "repository written to ${output_dir}"
find "$output_dir" -type f | sort | sed 's/^/  /'
