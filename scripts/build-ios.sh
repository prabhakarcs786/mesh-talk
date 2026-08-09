#!/usr/bin/env bash
# Builds the Rust mesh-mobile library for iOS, generates Swift bindings, packages an
# XCFramework, and (re)generates the Xcode project via XcodeGen.
#
# Usage: scripts/build-ios.sh [sim|device|both]   (default: sim)
#
# Requires: rustup (with aarch64-apple-ios-sim / aarch64-apple-ios targets installed),
# xcodegen (`brew install xcodegen`), and Xcode command line tools.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

MODE="${1:-sim}"

echo "==> Building mesh-mobile (release) for iOS..."
LIB_ARGS=()
case "$MODE" in
  sim)
    rustup target add aarch64-apple-ios-sim >/dev/null
    cargo build -p mesh-mobile --release --target aarch64-apple-ios-sim
    LIB_ARGS=(-library "target/aarch64-apple-ios-sim/release/libmesh_mobile.a" -headers ios/Frameworks/headers)
    BINDGEN_LIB="target/aarch64-apple-ios-sim/release/libmesh_mobile.dylib"
    ;;
  device)
    rustup target add aarch64-apple-ios >/dev/null
    cargo build -p mesh-mobile --release --target aarch64-apple-ios
    LIB_ARGS=(-library "target/aarch64-apple-ios/release/libmesh_mobile.a" -headers ios/Frameworks/headers)
    BINDGEN_LIB="target/aarch64-apple-ios/release/libmesh_mobile.dylib"
    ;;
  both)
    rustup target add aarch64-apple-ios-sim aarch64-apple-ios >/dev/null
    cargo build -p mesh-mobile --release --target aarch64-apple-ios-sim
    cargo build -p mesh-mobile --release --target aarch64-apple-ios
    LIB_ARGS=(
      -library "target/aarch64-apple-ios-sim/release/libmesh_mobile.a" -headers ios/Frameworks/headers
      -library "target/aarch64-apple-ios/release/libmesh_mobile.a" -headers ios/Frameworks/headers
    )
    BINDGEN_LIB="target/aarch64-apple-ios-sim/release/libmesh_mobile.dylib"
    ;;
  *)
    echo "Unknown mode '$MODE' (expected sim, device, or both)" >&2
    exit 1
    ;;
esac

echo "==> Generating Swift bindings..."
rm -rf bindings/swift
cargo run -p mesh-mobile --features uniffi-bindgen --bin uniffi-bindgen -- \
  generate --library "$BINDGEN_LIB" --language swift --out-dir bindings/swift

echo "==> Preparing headers for XCFramework..."
rm -rf ios/Frameworks/headers
mkdir -p ios/Frameworks/headers
cp bindings/swift/mesh_mobileFFI.h ios/Frameworks/headers/
cp bindings/swift/mesh_mobileFFI.modulemap ios/Frameworks/headers/module.modulemap

echo "==> Creating XCFramework..."
rm -rf ios/Frameworks/mesh_mobileFFI.xcframework
xcodebuild -create-xcframework "${LIB_ARGS[@]}" -output ios/Frameworks/mesh_mobileFFI.xcframework

echo "==> Generating Xcode project (XcodeGen)..."
(cd ios && xcodegen generate)

echo "==> Done. Open ios/MeshTalk.xcodeproj in Xcode, or build from the CLI, e.g.:"
echo "    xcodebuild -project ios/MeshTalk.xcodeproj -target MeshTalk -sdk iphonesimulator \\"
echo "      ARCHS=arm64 ONLY_ACTIVE_ARCH=NO CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO build"
