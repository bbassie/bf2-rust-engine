#!/usr/bin/env bash
# Builds the Linux server package: dist/bf2-rust-engine-<version>-linux-server.tar.gz with the
# dedicated server and bf2-import (the client isn't packaged for Linux), a player README and
# third-party notices. No BF2 content. Runs on the Linux build machine:
#
#   scripts/remote.sh scripts/package.sh            # leaves the package in ~/bf2-rust-engine/dist there
#   scripts/package.sh --skip-build                 # package existing release binaries
#   CARGO_PROFILE=dist scripts/package.sh           # thin LTO (much slower build)
#
# Copy it back with: scp -i ~/.ssh/bf2_build_ed25519 bbassie@192.168.4.204:bf2-rust-engine/dist/*.tar.gz dist/
#
# Fails if anything but the expected files ends up in the package, or anything that looks like
# converted BF2 content (see docs/ARCHITECTURE.md, "Packaging").
set -euo pipefail
cd "$(dirname "$0")/.."

profile="${CARGO_PROFILE:-release}"
target="${CARGO_TARGET_DIR:-target}"
version=$(awk '/^\[workspace\.package\]/{p=1; next} /^\[/{p=0} p && /^version *=/{gsub(/.*= *"|".*/, ""); print; exit}' Cargo.toml)
[[ -n "$version" ]] || { echo "no version in [workspace.package] of Cargo.toml"; exit 1; }
name="bf2-rust-engine-$version-linux-server"
echo "Packaging $name ($profile build)"

if [[ "$profile" == release ]]; then profile_args=(--release); else profile_args=(--profile "$profile"); fi
if [[ "${1:-}" != "--skip-build" ]]; then
    # One package per invocation: together they would unify Bevy features.
    for package in bf2_import game_server; do
        echo "cargo build ${profile_args[*]} -p $package"
        cargo build "${profile_args[@]}" -p "$package"
    done
fi

stage="dist/$name"
rm -rf "$stage"
mkdir -p "$stage"
for bin in server bf2-import; do
    [[ -f "$target/$profile/$bin" ]] || { echo "missing $target/$profile/$bin"; exit 1; }
    cp "$target/$profile/$bin" "$stage/"
done
sed "s/{VERSION}/$version/g" scripts/package/README.txt > "$stage/README.txt"
cat > "$stage/LICENSE.txt" <<EOF
bf2-rust-engine $version
Licensed under either of the MIT license or the Apache License, Version 2.0, at your option
(SPDX: MIT OR Apache-2.0).

Battlefield 2 is a trademark of Electronic Arts. This program contains no EA code or assets.
EOF

# Third-party crates in the two binaries (normal and build dependencies, not dev).
cargo metadata --format-version 1 --locked --filter-platform x86_64-unknown-linux-gnu |
    python3 scripts/package/third_party.py "$version" bf2_import game_server > "$stage/THIRD-PARTY-NOTICES.txt"

# Only our files, nothing converted from BF2.
problems=0
while IFS= read -r -d '' path; do
    rel="${path#"$stage"/}"
    case "$rel" in
        server|bf2-import|README.txt|LICENSE.txt|THIRD-PARTY-NOTICES.txt) ;;
        *) echo "unexpected file $rel"; problems=1 ;;
    esac
    case "${rel,,}" in
        *.glb|*.gltf|*.dds|*.tga|*.ogg|*.wav|*.bin|*.zip|*.con|*.raw|*.ron|*.json|*mesh|*.baf|*.ske|*.tai)
            echo "BF2-derived file type $rel"; problems=1 ;;
    esac
    if [[ "$rel" == *.txt ]] && grep -q "Converted from a local Battlefield 2 installation" "$path"; then
        echo "bf2-import output marker in $rel"; problems=1
    fi
done < <(find "$stage" -mindepth 1 \( -type f -o -type l \) -print0)
if find "$stage" -mindepth 1 -type d | grep -q .; then
    echo "unexpected folders:"; find "$stage" -mindepth 1 -type d; problems=1
fi
(( problems == 0 )) || { echo "the package has files that must not be distributed (above)"; exit 1; }

tar -C dist -czf "dist/$name.tar.gz" "$name"
ls -l "$stage"
echo "dist/$name.tar.gz ($(( $(stat -c %s "dist/$name.tar.gz") / 1048576 )) MB)"
