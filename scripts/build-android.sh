#!/bin/sh
# Builds gnaudio for Android with the NDK, without cargo-ndk: the shared library
# the app loads, and the device probe, for arm64-v8a — the one ABI the app ships.
#
#   scripts/build-android.sh            # release build, into dist/arm64-v8a/
#   scripts/build-android.sh --debug    # an unoptimised build
#
# The NDK is found at $ANDROID_NDK_HOME, or the newest one under the SDK. The API
# level is the app's own floor: AAudio is API 26, and GameNative's minSdk is 26.
#
# Every artifact is linked for a 16 KB page size by `.cargo/config.toml` and then
# checked by `scripts/check-16kb.sh`, which fails this script rather than letting a
# library that only loads on 4 KB devices reach `dist/`.

set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
profile=release
# Empty in a debug build, so it expands to nothing.
profile_flag=""
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done
[ "$profile" = "release" ] && profile_flag="--release"

api=26
target=aarch64-linux-android
abi=arm64-v8a
# clang picks the triple, the API level and the sysroot from its own name.
cc=aarch64-linux-android

ndk=${ANDROID_NDK_HOME:-}
if [ -z "$ndk" ]; then
    sdk=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}
    ndk=$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1)
fi
if [ -z "$ndk" ] || [ ! -d "$ndk" ]; then
    echo "no NDK found: set ANDROID_NDK_HOME, or install one under \$ANDROID_SDK_ROOT/ndk" >&2
    exit 1
fi
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/$cc$api-clang"

cd "$root"
echo "=== $target ($abi), API $api, $profile"
cargo build --target "$target" ${profile_flag:+$profile_flag} --lib --example gn-audio-probe --example gn-audio-rate --example aaudio-direct

# Checked where cargo wrote them, before anything reaches dist/. A published artifact is
# the one thing a caller trusts, so a 4 KB build must never land there — not even for the
# moment between a copy and a check that fails afterwards.
echo
echo "=== 16 KB page size"
"$root/scripts/check-16kb.sh" \
    "target/$target/$profile/libgnaudio.so" \
    "target/$target/$profile/examples/gn-audio-probe" \
    "target/$target/$profile/examples/gn-audio-rate" \
    "target/$target/$profile/examples/aaudio-direct"

mkdir -p "dist/$abi"
cp "target/$target/$profile/libgnaudio.so" "dist/$abi/"
cp "target/$target/$profile/examples/gn-audio-probe" "dist/$abi/"
cp "target/$target/$profile/examples/gn-audio-rate" "dist/$abi/"
cp "target/$target/$profile/examples/aaudio-direct" "dist/$abi/"

echo
echo "built:"
md5sum "dist/$abi/libgnaudio.so" "dist/$abi/gn-audio-probe" "dist/$abi/gn-audio-rate" "dist/$abi/aaudio-direct"
exit 0
