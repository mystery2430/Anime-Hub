#!/usr/bin/env bash
#
# Local packaging for AnimeHub.
#
#   ./scripts/build.sh test      # frontend + Rust tests
#   ./scripts/build.sh linux     # AppImage + .deb + .rpm
#   ./scripts/build.sh windows   # NSIS installer (run on Windows)
#   ./scripts/build.sh android   # APKs
#   ./scripts/build.sh icon      # regenerate src-tauri/icon.png + all sizes
#
# Distribution builds use the tuned `release` profile (LTO on), which peaks at
# well over 2 GB of RAM while linking the GTK/WebKit crates. On a small
# machine prefix the command with TAURI_LOW_MEMORY=1: the release profile is
# relaxed for the duration of the build and restored afterwards.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Rust is commonly installed into ~/.cargo/bin and is not on a
# non-interactive shell's PATH.
export PATH="$HOME/.cargo/bin:$PATH"

TAURI="./node_modules/.bin/tauri"
CARGO_TOML="src-tauri/Cargo.toml"
PROFILE_BACKUP=""

restore_profile() {
  if [[ -n "$PROFILE_BACKUP" && -f "$PROFILE_BACKUP" ]]; then
    mv -f "$PROFILE_BACKUP" "$CARGO_TOML"
  fi
}
trap restore_profile EXIT

if [[ "${TAURI_LOW_MEMORY:-0}" == "1" ]]; then
  PROFILE_BACKUP="$(mktemp)"
  cp "$CARGO_TOML" "$PROFILE_BACKUP"
  python3 scripts/_relax_profile.py "$CARGO_TOML"
  echo "note: low-memory release profile in effect (restored on exit)"
fi

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing dependency: $1" >&2
    exit 1
  }
}

cmd="${1:-test}"

case "$cmd" in
  test)
    need node
    need cargo
    npm test
    cargo test --all --manifest-path src-tauri/Cargo.toml
    echo "all tests passed"
    ;;

  linux)
    need cargo
    need npm
    if ! pkg-config --exists webkit2gtk-4.1; then
      echo "libwebkit2gtk-4.1-dev is required; see README" >&2
      exit 1
    fi
    $TAURI build --bundles appimage,deb,rpm
    ;;

  windows)
    need cargo
    need npm
    $TAURI build --bundles nsis
    ;;

  android)
    need cargo
    need npm
    : "${ANDROID_HOME:?set ANDROID_HOME to your Android SDK}"
    : "${NDK_HOME:?set NDK_HOME to your NDK}"
    [[ -d src-tauri/gen/android ]] || $TAURI android init
    # init does not know about the Kotlin bridge or the PiP manifest flag.
    python3 scripts/android_prepare.py
    # Signing is picked up from gen/android/keystore.properties if present;
    # otherwise the APK is unsigned. Never commit the keystore.
    if [[ "${ANDROID_SPLIT:-0}" == "1" ]]; then
      $TAURI android build --apk --split-per-abi
    else
      $TAURI android build --apk
    fi
    ;;

  icon)
    need python3
    python3 scripts/make_icon.py
    $TAURI icon src-tauri/icon.png --output src-tauri/icons
    ;;

  *)
    sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
