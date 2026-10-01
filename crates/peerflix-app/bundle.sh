#!/bin/sh
# Builds peerflix.app into target/app, from the app profile's build.
set -eu
cd "$(dirname "$0")/../.."
cargo build --profile app --locked -p peerflix-app
# The crate version, for builds that aren't releases.
version=$(cargo pkgid -p peerflix-app | sed 's/.*[#@]//')
version=${PEERFLIX_VERSION:-$version}
version=${version#v}
app=target/app/peerflix.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/app/peerflix-app "$app/Contents/MacOS/"
sed "s/VERSION/$version/g" crates/peerflix-app/Info.plist > "$app/Contents/Info.plist"
# Ad hoc, as there's no Developer ID to sign with.
codesign --force --sign - "$app"
echo "$app"
