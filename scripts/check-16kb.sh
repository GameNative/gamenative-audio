#!/bin/sh
# Fails unless every file given is loadable on a 16 KB page size device.
#
#   scripts/check-16kb.sh dist/arm64-v8a/*.so
#   scripts/check-16kb.sh dist/arm64-v8a
#
# What this checks, and why each one is a real failure rather than a nicety:
#
#   * **Every `PT_LOAD` segment is aligned to 16 KB.** This is the requirement. Android
#     15 shipped devices with a 16 KB kernel page size, and the dynamic linker maps a
#     library's loadable segments at their `p_align` granularity. A 4 KB-aligned library
#     cannot be mapped on such a device: `dlopen` fails, and for a Wine unixlib — which
#     Wine loads with its own loader, during container start — the container comes up
#     with no audio and no obvious cause. A build that is correct on every 4 KB device
#     and wrong on every 16 KB one is the whole shape of this bug.
#
#   * **No `TEXTREL`.** Text relocations need the code segment writable at load, which a
#     larger page size makes impossible to honour; a library carrying them cannot be
#     fixed by alignment alone. Nothing here has ever produced one, and this is what
#     would notice if something started to.
#
# It reads ELF headers rather than trusting the build: a linker given
# `-Wl,-z,max-page-size=16384` and ignoring it emits a perfectly ordinary-looking file,
# and `file`/`nm`/`md5sum` all report it as fine.

set -eu

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <file-or-directory>..." >&2
    exit 2
fi

# A readelf from the NDK, or the host's. Either understands ELF program headers; the
# NDK's is preferred because it is the same LLVM version that links these files.
readelf=""
ndk=${ANDROID_NDK_HOME:-}
if [ -z "$ndk" ]; then
    sdk=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}}
    ndk=$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1)
fi
if [ -n "$ndk" ] && [ -x "$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf" ]; then
    readelf="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
elif command -v llvm-readelf >/dev/null 2>&1; then
    readelf=llvm-readelf
elif command -v readelf >/dev/null 2>&1; then
    readelf=readelf
else
    echo "no readelf found: set ANDROID_NDK_HOME, or install one" >&2
    exit 1
fi

# Expands directories one level into the regular files they hold.
files=""
for argument in "$@"; do
    if [ -d "$argument" ]; then
        for entry in "$argument"/*; do
            [ -f "$entry" ] && files="$files $entry"
        done
    elif [ -f "$argument" ]; then
        files="$files $argument"
    else
        echo "no such file or directory: $argument" >&2
        exit 2
    fi
done

status=0
for file in $files; do
    # ELF magic, as four bytes: anything else here is not ours to judge.
    if [ "$(dd if="$file" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')" != "7f454c46" ]; then
        echo "skip  $file (not an ELF file)"
        continue
    fi

    # Every PT_LOAD alignment in the file. One misaligned segment is enough to make the
    # library unloadable, so this looks at all of them rather than the smallest.
    alignments=$("$readelf" -lW "$file" | awk '$1 == "LOAD" { print $NF }')

    if [ -z "$alignments" ]; then
        echo "FAIL  $file (no LOAD segments: not a loadable object)"
        status=1
        continue
    fi

    # Compared in the shell's own arithmetic, which reads a `0x` prefix, rather than in
    # awk: `strtonum` is a gawk extension absent from mawk and busybox awk, and
    # `sort -n` reads every `0x…` as zero and orders them by input position, so either
    # would leave this gate silently passing a 4 KB library on the machines that do not
    # have gawk.
    worst=""
    low=""
    for alignment in $alignments; do
        # 0x4000 is 16384, and every segment has to reach it.
        value=$(($alignment)) 2>/dev/null || value=0
        if [ -z "$low" ] || [ "$value" -lt "$low" ]; then
            low=$value
            low_hex=$alignment
        fi
        if [ -z "$worst" ] || [ "$value" -gt "$worst" ]; then
            worst=$value
            worst_hex=$alignment
        fi
    done

    if [ "$low" -lt 16384 ]; then
        echo "FAIL  $file (a LOAD segment is aligned to $low_hex, not 0x4000)"
        status=1
        continue
    fi

    if "$readelf" -dW "$file" | grep -q "TEXTREL"; then
        echo "FAIL  $file (carries TEXTREL: text relocations cannot be honoured)"
        status=1
        continue
    fi

    echo "ok    $file (LOAD align $worst_hex, no TEXTREL)"
done

if [ "$status" -ne 0 ]; then
    echo >&2
    echo "at least one file is not loadable on a 16 KB page size device" >&2
fi
exit "$status"
