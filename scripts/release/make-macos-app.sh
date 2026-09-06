#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Fabian Schmieder
#
# Assemble devserial.app from the two macOS release binaries.
#
# The bundle carries the same program as the archives, fused into one universal
# binary. Launched from the Finder it gets no command and no terminal on stdin;
# src/cli/mod.rs recognises the bundle by its own path and opens the GUI instead
# of starting an MCP server nobody can see.
#
# This script only builds the tree. Signing, notarization and stapling happen in
# the release workflow, which has the credentials; the result of this script is
# their input and is deliberately reproducible on its own.
#
# Usage:
#   make-macos-app.sh --version 1.2.3 \
#                     --arm64 path/to/devserial --x86_64 path/to/devserial \
#                     --output-dir dist
#
# Produces "$output_dir/devserial.app".

set -euo pipefail

BUNDLE_ID="com.metaneutrons.devserial"
MIN_MACOS="11.0"

die() { printf '%s\n' "$*" >&2; exit 1; }

version=""; arm64_bin=""; x86_64_bin=""; output_dir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)    version=${2:?}; shift 2 ;;
    --arm64)      arm64_bin=${2:?}; shift 2 ;;
    --x86_64)     x86_64_bin=${2:?}; shift 2 ;;
    --output-dir) output_dir=${2:?}; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$version" ]]    || die "--version is required"
[[ -n "$output_dir" ]] || die "--output-dir is required"
[[ -f "$arm64_bin" ]]  || die "--arm64 does not name a file: $arm64_bin"
[[ -f "$x86_64_bin" ]] || die "--x86_64 does not name a file: $x86_64_bin"

# CFBundleShortVersionString takes at most three dot-separated numbers, so a
# prerelease suffix has to come off. The full tag stays the release identity
# everywhere else; here it would make the bundle unreadable to Launch Services.
short_version=${version%%-*}
[[ "$short_version" =~ ^[0-9]+(\.[0-9]+){0,2}$ ]] \
  || die "cannot derive CFBundleShortVersionString from '$version'"

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
icon="$repo_root/resources/icon.icns"
[[ -f "$icon" ]] || die "the bundle icon is missing: $icon"

app="$output_dir/devserial.app"
rm -rf -- "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# One universal binary, not two bundles. `lipo` discards the per-architecture
# ad-hoc signature the linker applied, so the result is unsigned here and gets
# its Developer ID signature in the workflow.
lipo -create -output "$app/Contents/MacOS/devserial" "$arm64_bin" "$x86_64_bin"
chmod 0755 "$app/Contents/MacOS/devserial"

cp "$icon" "$app/Contents/Resources/icon.icns"
chmod 0644 "$app/Contents/Resources/icon.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>devserial</string>
	<key>CFBundleIconFile</key>
	<string>icon</string>
	<key>CFBundleIdentifier</key>
	<string>${BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>devserial</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${short_version}</string>
	<key>CFBundleVersion</key>
	<string>${short_version}</string>
	<key>LSMinimumSystemVersion</key>
	<string>${MIN_MACOS}</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>Copyright (C) 2026 Fabian Schmieder. GPL-3.0-or-later.</string>
</dict>
</plist>
PLIST
chmod 0644 "$app/Contents/Info.plist"

# What is claimed above has to hold for the tree that was just written.
plutil -lint "$app/Contents/Info.plist" >/dev/null
for arch in arm64 x86_64; do
  lipo -archs "$app/Contents/MacOS/devserial" | tr ' ' '\n' | grep -qx "$arch" \
    || die "the universal binary is missing $arch"
done
"$app/Contents/MacOS/devserial" --version >/dev/null

printf '%s\n' "$app"
