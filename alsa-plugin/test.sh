#!/bin/sh
# Builds the ALSA tone, installs the plugin on the device, and plays it there.
#
#   alsa-plugin/test.sh          # build, push, run
#   alsa-plugin/test.sh --build  # build only
#
# What this answers, and what it cannot: it proves that libasound loads the plugin,
# negotiates parameters with it, and that the engine plays the result — with no Wine
# anywhere in the path. It says nothing about whether a game reaches it; that is the
# next test, and keeping the two apart is the point.
#
# Everything runs as the app's own uid (`run-as`), because AudioFlinger and AAudio
# need a real Android process context: a plain `adb shell` process has no package and
# no audio permission, and would fail in a way that looks like a plugin fault.

set -eu

root=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$root/.." && pwd)
serial=${ANDROID_SERIAL:-3a59aa7b}
pkg=${GNAUDIO_PKG:-app.gamenative}
api=26
target=aarch64-linux-android

adb() { "$HOME/Android/Sdk/platform-tools/adb" -s "$serial" "$@"; }

ndk=${ANDROID_NDK_HOME:-}
if [ -z "$ndk" ]; then
    sdk=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}
    ndk=$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1)
fi
toolchain="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
cc="$toolchain/aarch64-linux-android$api-clang"

# The link needs a real libasound, not the stub the plugin is built against: this
# binary calls into it at run time, so the symbols have to be known here.
real="$root/prebuilt/real"
mkdir -p "$real" "$root/build"
if [ ! -f "$real/libasound.so.2" ]; then
    # The tone must be linked against the same libasound the device runs, which is the
    # image's own copy — pull it rather than checking a library into the tree. `exec-out
    # run-as cat` streams it out as the app's uid, because /data/local/tmp is the shell's
    # and not the app's.
    echo "=== fetching the image's libasound.so.2 for linking"
    "$HOME/Android/Sdk/platform-tools/adb" -s "$serial" exec-out \
        run-as "$pkg" cat files/imagefs/usr/lib/libasound.so.2 > "$real/libasound.so.2"
    if [ ! -s "$real/libasound.so.2" ]; then
        echo "could not pull the image's libasound.so.2 into $real" >&2
        rm -f "$real/libasound.so.2"
        exit 1
    fi
fi
ln -sf libasound.so.2 "$real/libasound.so"

echo "=== tone, $target"
# `$ORIGIN` in the rpath, not LD_LIBRARY_PATH: the device test copies the tone and the
# libraries it needs into one directory and runs it from there. A relative
# LD_LIBRARY_PATH is dropped by the Android linker without a word, and an absolute one
# would bake a data-dir path into the binary.
rustc --edition 2021 -O --target "$target" \
    -L "$real" \
    -C "linker=$cc" \
    -C "link-arg=-Wl,-rpath,\$ORIGIN" \
    -o "$root/build/tone" "$root/test/tone.rs"

if [ "${1:-}" = "--build" ]; then
    echo "built: $root/build/tone"
    exit 0
fi

echo "=== plugin"
"$root/build.sh" >/dev/null
plugin="$repo/dist/arm64-v8a/libasound_module_pcm_gamenative.so"

dev="$pkg/files/tmp/gnalsa"
echo "=== push to $dev"
adb shell "run-as $pkg mkdir -p files/tmp/gnalsa"
adb push "$root/build/tone" /data/local/tmp/gnalsa-tone >/dev/null
adb push "$plugin" /data/local/tmp/gnalsa-plugin.so >/dev/null
adb push "$root/test/test-alsa.conf" /data/local/tmp/gnalsa-alsa.conf >/dev/null
# Each file keeps the name libasound looks for: `type gamenative` loads
# `libasound_module_pcm_gamenative.so` from ALSA_PLUGIN_DIR by construction.
adb shell "run-as $pkg cp /data/local/tmp/gnalsa-tone files/tmp/gnalsa/tone"
adb shell "run-as $pkg cp /data/local/tmp/gnalsa-plugin.so files/tmp/gnalsa/libasound_module_pcm_gamenative.so"
adb shell "run-as $pkg cp /data/local/tmp/gnalsa-alsa.conf files/tmp/gnalsa/test-alsa.conf"
adb shell "run-as $pkg chmod 755 files/tmp/gnalsa/tone"
# The tone needs libasound, and libasound needs Android's two System V shims; all of
# them go beside the binary, which is what the $ORIGIN rpath is for.
for lib in libasound.so.2 libandroid-sysv-semaphore.so libandroid-sysvshm.so; do
    adb shell "run-as $pkg cp files/imagefs/usr/lib/$lib files/tmp/gnalsa/$lib"
done
adb shell "rm -f /data/local/tmp/gnalsa-tone /data/local/tmp/gnalsa-plugin.so /data/local/tmp/gnalsa-alsa.conf"

echo "=== run"
adb logcat -c
# `run-as` starts in the app's data dir; the absolute form is what the linker needs to
# resolve libasound's own dependencies (Android's SysV shims), which an $ORIGIN rpath
# on the executable does not cover.
dev_abs=$(adb shell "run-as $pkg pwd" | tr -d '\r')/files/tmp/gnalsa
# LD_LIBRARY_PATH reaches the image's own libasound (and the two Android shims it
# needs); ALSA_PLUGIN_DIR is where the plugin is, since the image's compiled-in
# plugin path is an absolute /usr/lib that is not where we are running from.
# `run-as` starts in the app's data dir, so `$PWD` is the absolute path ALSA_PLUGIN_DIR
# and ALSA_CONFIG_PATH need.
adb shell "run-as $pkg env \
    LD_LIBRARY_PATH=$dev_abs \
    ALSA_PLUGIN_DIR=$dev_abs \
    ALSA_CONFIG_PATH=$dev_abs/test-alsa.conf \
    ${GNAUDIO_ALSA_DEBUG:+GNAUDIO_ALSA_DEBUG=1} \
    $dev_abs/tone ${GNAUDIO_ROUNDS:-12}" 2>&1 | tail -40

echo
echo "=== logcat (gnaudio / gnaudio-alsa)"
adb logcat -d -s gnaudio gnaudio-alsa 2>&1 | tail -25
exit 0
