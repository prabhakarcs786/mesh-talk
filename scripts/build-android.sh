#!/usr/bin/env bash
# Builds the Rust mesh-mobile library for Android (all 4 ABIs), generates Kotlin
# bindings, and stages both into android/app/src/main/{jniLibs,java} ready for Gradle.
#
# Usage: scripts/build-android.sh
#
# Requires: rustup (with the 4 android targets), the Android SDK + NDK (this script
# defaults to Homebrew's android-commandlinetools location; override with ANDROID_HOME /
# ANDROID_NDK_HOME if yours lives elsewhere).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
NDK_VERSION="${NDK_VERSION:-27.0.12077973}"
ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/$NDK_VERSION}"
NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin"
API_LEVEL=21

if [ ! -d "$NDK_BIN" ]; then
  echo "error: NDK not found at $NDK_BIN" >&2
  echo "  install with: sdkmanager --install 'ndk;$NDK_VERSION'" >&2
  exit 1
fi

# rust target triple -> (android ABI dir name, NDK clang triple prefix)
TARGETS=(
  "aarch64-linux-android:arm64-v8a:aarch64-linux-android"
  "armv7-linux-androideabi:armeabi-v7a:armv7a-linux-androideabi"
  "x86_64-linux-android:x86_64:x86_64-linux-android"
  "i686-linux-android:x86:i686-linux-android"
)

JNI_LIBS_DIR="android/app/src/main/jniLibs"
rm -rf "$JNI_LIBS_DIR"

for entry in "${TARGETS[@]}"; do
  IFS=":" read -r rust_target abi clang_prefix <<< "$entry"
  clang="$NDK_BIN/${clang_prefix}${API_LEVEL}-clang"
  env_target=$(echo "$rust_target" | tr '-' '_')

  echo "==> Building mesh-mobile for $rust_target ($abi)..."
  rustup target add "$rust_target" >/dev/null

  env \
    "CARGO_TARGET_$(echo "$env_target" | tr '[:lower:]' '[:upper:]')_LINKER=$clang" \
    "CC_${env_target}=$clang" \
    "AR_${env_target}=$NDK_BIN/llvm-ar" \
    cargo build -p mesh-mobile --release --target "$rust_target"

  mkdir -p "$JNI_LIBS_DIR/$abi"
  cp "target/$rust_target/release/libmesh_mobile.so" "$JNI_LIBS_DIR/$abi/"
done

echo "==> Generating Kotlin bindings..."
rm -rf bindings/kotlin
cargo run -p mesh-mobile --features uniffi-bindgen --bin uniffi-bindgen -- \
  generate --library target/aarch64-linux-android/release/libmesh_mobile.so \
  --language kotlin --out-dir bindings/kotlin

KOTLIN_SRC_DIR="android/app/src/main/java"
rm -rf "$KOTLIN_SRC_DIR/uniffi"
mkdir -p "$KOTLIN_SRC_DIR"
cp -r bindings/kotlin/uniffi "$KOTLIN_SRC_DIR/uniffi"

echo "==> Done. jniLibs staged under $JNI_LIBS_DIR, Kotlin bindings under $KOTLIN_SRC_DIR/uniffi."
echo "    Build the app with: (cd android && JAVA_HOME=\$(/usr/libexec/java_home -v 17) ./gradlew assembleDebug)"
