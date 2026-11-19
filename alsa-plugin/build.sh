#!/bin/sh
# Builds libasound_module_pcm_gamenative.so: the C shim, then the Rust crate, linked
# into one Android (bionic) shared object.
#
#   alsa-plugin/build.sh            # release, into dist/arm64-v8a/
#   alsa-plugin/build.sh --debug
#
# Three steps, in this order, and each one exists for a reason:
#
#   1. A stub libasound, so the shared object records `libasound.so.2` in its
#      DT_NEEDED (see shim/libasound-stub.c for why that is not optional).
#   2. The C shim, compiled with `-DPIC` — the macro, not just `-fPIC`, because
#      `alsa/global.h` keys on it and the other branch references `snd_dlsym_start`,
#      which no libasound exports.
#   3. The Rust crate, with the shim's object file as a linker input. The plugin is
#      one .so: the engine is a static dependency of it, so there is nothing else to
#      copy to the device.
#
# The entry point itself is defined in Rust (src/entry.rs) for one reason: rustc
# links a cdylib with a version script listing only the crate's own exports, and a
# symbol a companion object file defines is localised by it — present, and invisible
# to `dlsym`. Keeping the mangled name on the Rust side is what makes it exportable.

set -eu

root=$(cd "$(dirname "$0")" && pwd)
profile=release
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

api=26
target=aarch64-linux-android
abi=arm64-v8a

ndk=${ANDROID_NDK_HOME:-}
if [ -z "$ndk" ]; then
    sdk=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}
    ndk=$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1)
fi
if [ -z "$ndk" ] || [ ! -d "$ndk" ]; then
    echo "no NDK found: set ANDROID_NDK_HOME, or install one under \$ANDROID_SDK_ROOT/ndk" >&2
    exit 1
fi

toolchain="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
cc="$toolchain/aarch64-linux-android$api-clang"
if [ ! -x "$cc" ]; then
    echo "no compiler at $cc" >&2
    exit 1
fi
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$cc"

mkdir -p "$root/build" "$root/prebuilt"

echo "=== link stub, libasound.so.2"
"$cc" -shared -Wl,-soname,libasound.so.2 -o "$root/prebuilt/libasound.so.2" \
    "$root/shim/libasound-stub.c"

echo "=== shim, gamenative.o"
"$cc" -c -O2 -fPIC -DPIC -Wall -Wextra -Werror -I"$root/include" \
    -o "$root/build/gamenative.o" "$root/shim/gamenative.c"

echo "=== crate, $target ($abi), API $api, $profile"
# build.rs declares shim/gamenative.c and build/gamenative.o as inputs, so a change to
# either one relinks. Without that, Cargo sees no change in the Rust crate and the stale
# .so is published to dist/ looking freshly built.
cd "$root"
cargo_extra=""
[ "$profile" = "release" ] && cargo_extra="--release"
cargo rustc --target "$target" $cargo_extra --lib -- \
    -C "link-arg=$root/build/gamenative.o" \
    -C "link-arg=$root/prebuilt/libasound.so.2" \
    -C "link-arg=-Wl,--no-undefined"

built="target/$target/$profile/libgamenative_alsa_plugin.so"
if [ ! -f "$built" ]; then
    echo "no plugin at $built" >&2
    exit 1
fi

mkdir -p "$root/../dist/$abi"
out="$root/../dist/$abi/libasound_module_pcm_gamenative.so"
cp "$built" "$out"
# Stripping is part of the profile, but a link that failed to keep the entry point
# would still produce a plausible-looking file; check rather than assume.
if ! "$toolchain/llvm-nm" -D --defined-only "$out" | grep -q "_snd_pcm_gamenative_open"; then
    echo "the built plugin does not export _snd_pcm_gamenative_open" >&2
    exit 1
fi
if ! "$toolchain/llvm-readelf" -d "$out" | grep -q "libasound.so.2"; then
    echo "the built plugin does not depend on libasound.so.2" >&2
    exit 1
fi

echo
echo "built:"
md5sum "$out"
echo "  $abi/libasound_module_pcm_gamenative.so"
exit 0
