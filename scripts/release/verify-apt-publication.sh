#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Fabian Schmieder
#
# Verify the APT archive as an apt client reads it: InRelease points to each
# architecture's SHA512 by-hash index, which names the delivered .deb. Mutable
# Packages.gz URLs are cached by the CDN and cannot establish freshness.

set -euo pipefail

version=''
tag=''
timeout_seconds=1200
base_url='https://deb.metaneutrons.cc'

die() { echo "verify-apt-publication: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) version=$2; shift 2 ;;
    --tag) tag=$2; shift 2 ;;
    --timeout-seconds) timeout_seconds=$2; shift 2 ;;
    --base-url) base_url=$2; shift 2 ;;
    *) die "unknown argument '$1'" ;;
  esac
done

[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  die 'version must be a canonical SemVer core'
[[ "$tag" == "devserial-v${version}" ]] || die 'tag does not match version'
[[ "$timeout_seconds" =~ ^(0|[1-9][0-9]*)$ ]] || die 'invalid timeout'
[[ "$base_url" =~ ^https?://[^/]+$ ]] || die 'base URL must be a host without a path'

work=$(mktemp -d)
trap 'rm -r -- "$work"' EXIT
index_base="${base_url}/dists/rolling"
expected_version="${version}-1"

index_sha() {
  local arch=$1
  awk -v want="main/binary-${arch}/Packages.gz" '
    /^SHA512:/ { block = 1; next }
    block && /^[^[:space:]]/ { block = 0 }
    block && $3 == want { print $1 }
  ' "$work/InRelease"
}

package_path() {
  local arch=$1
  awk -v package=devserial -v version="$expected_version" -v arch="$arch" '
    $1 == "Package:"      { pkg = $2 }
    $1 == "Version:"      { ver = $2 }
    $1 == "Architecture:" { architecture = $2 }
    $1 == "Filename:"     { filename = $2 }
    function finish() {
      if (pkg == package && ver == version && architecture == arch) {
        hits++
        path = filename
      }
      pkg = ver = architecture = filename = ""
    }
    /^[[:space:]]*$/ { finish() }
    END {
      finish()
      if (hits == 1 && path != "") print path
      else if (hits > 1) exit 2
    }
  ' "$work/packages-${arch}"
}

resolve_arch() {
  local arch=$1 sum actual path
  sum=$(index_sha "$arch") || return 1
  [[ "$sum" =~ ^[[:xdigit:]]{128}$ ]] || return 1
  curl --fail --silent --show-error --location --max-time 30 \
    "${index_base}/main/binary-${arch}/by-hash/SHA512/${sum}" \
    --output "$work/packages-${arch}.gz" || return 1
  actual=$(openssl dgst -sha512 "$work/packages-${arch}.gz" | awk '{ print $NF }') || return 1
  [[ "${actual,,}" == "${sum,,}" ]] || return 1
  gzip -t "$work/packages-${arch}.gz" || return 1
  gzip -dc "$work/packages-${arch}.gz" > "$work/packages-${arch}"
  path=$(package_path "$arch") || return 1
  [[ "$path" == pool/* && "$path" != *'/../'* ]] || return 1
  printf '%s' "$path"
}

deadline=$((SECONDS + timeout_seconds))
delay=5
attempt=0
while :; do
  attempt=$((attempt + 1))
  if curl --fail --silent --show-error --location --max-time 30 \
      "${index_base}/InRelease" --output "$work/InRelease"; then
    amd64=$(resolve_arch amd64) && arm64=$(resolve_arch arm64) && break
  fi
  if (( SECONDS >= deadline )); then
    die "the archive did not serve devserial ${expected_version} on both architectures within ${timeout_seconds}s; inspect the apt-archive publish workflow"
  fi
  echo "archive has not converged to devserial ${expected_version} (attempt ${attempt}); retrying in ${delay}s"
  remaining=$((deadline - SECONDS))
  (( delay < remaining )) || delay=$remaining
  sleep "$delay"
  (( delay < 45 )) && delay=$((delay * 2))
  (( delay > 45 )) && delay=45
done

mkdir -p "$work/assets"
for arch in amd64 arm64; do
  case "$arch" in
    amd64) path=$amd64 ;;
    arm64) path=$arm64 ;;
  esac
  asset="devserial_${expected_version}_${arch}.deb"
  curl --fail --silent --show-error --location --max-time 60 \
    "${base_url}/${path}" --output "$work/published-${arch}.deb"
  gh release download "$tag" --repo metaneutrons/devserial \
    --pattern "$asset" --dir "$work/assets" --clobber
  cmp "$work/published-${arch}.deb" "$work/assets/$asset" ||
    die "the public ${arch} package differs from the release asset"
  echo "verified public ${arch} index and package: ${asset}"
done
