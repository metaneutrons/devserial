#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Fabian Schmieder
#
# Build the Debian package.
#
# Two files a Debian package is expected to carry cannot be checked in: the
# manual page is derived from the program's own --help, and the Debian
# changelog needs the version of the release being built. Both are generated
# here, which is why the package is built through this script rather than by
# calling cargo-deb directly.
#
# Usage: build-deb.sh --version 1.2.3 --output-dir deb

set -euo pipefail

version=''
output_dir='deb'
maintainer='metaneutrons <metaneutrons@users.noreply.github.com>'

die() { echo "build-deb: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) version=$2; shift 2 ;;
    --output-dir) output_dir=$2; shift 2 ;;
    *) die "unknown argument '$1'" ;;
  esac
done

[[ -n "$version" ]] || die 'missing --version'
binary=target/release/devserial
[[ -x "$binary" ]] || die "build the release binary first, '$binary' is missing"

extra=target/deb-extra
rm -rf "$extra"
mkdir -p "$extra" "$output_dir"

# The manual page, from the program's own help output. The locale matters:
# without a UTF-8 one, help2man mangles every non-ASCII character in the help
# text into question marks.
command -v help2man >/dev/null 2>&1 || die 'help2man is not installed'
export LC_ALL=C.UTF-8
help2man --no-info --no-discard-stderr \
  --name 'serial hardware bridge for LLMs, scripts and humans' \
  --source "devserial ${version}" \
  "./${binary}" > "${extra}/devserial.1"
gzip -9n "${extra}/devserial.1"

# The Debian changelog. One entry per release, pointing at the upstream notes.
date=$(date -R)
cat > "${extra}/changelog.Debian" <<CHANGELOG
devserial (${version}-1) stable; urgency=medium

  * Release ${version}. The upstream release notes are at
    https://github.com/metaneutrons/devserial/releases/tag/devserial-v${version}

 -- ${maintainer}  ${date}
CHANGELOG
gzip -9n "${extra}/changelog.Debian"

cargo deb --no-build --output "$output_dir"

echo "built into ${output_dir}"
ls -la "$output_dir"
