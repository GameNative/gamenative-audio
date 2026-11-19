#!/bin/sh
# Packs the plugin and its ALSA config into the asset GameNative ships.
#
#   scripts/pack-alsa-asset.sh            # write the asset
#   scripts/pack-alsa-asset.sh --check    # verify the asset is current, change nothing
#
# The payload is deliberately tiny — two files, no Wine-side artifact at all:
#
#   alsa-lib/libasound_module_pcm_gamenative.so   the plugin, engine included
#   gamenative.conf                               the ALSA_CONFIG_PATH wrapper
#
# `--check` is what makes a stale payload impossible: the asset is compared byte for
# byte against what a rebuild would produce, so a "tested" build cannot be an old one.

set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
asset=${GNAUDIO_ASSET:-/data/workspace/Projects/GameNative/app/src/main/assets/gamenative-audio-guest.tzst}
plugin="$root/dist/arm64-v8a/libasound_module_pcm_gamenative.so"
conf="$root/alsa-plugin/conf/gamenative.conf"

check=0
for arg in "$@"; do
    case "$arg" in
        --check) check=1 ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

for file in "$plugin" "$conf"; do
    if [ ! -f "$file" ]; then
        echo "missing $file — run alsa-plugin/build.sh first" >&2
        exit 1
    fi
done

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/alsa-lib"
cp "$plugin" "$stage/alsa-lib/libasound_module_pcm_gamenative.so"
cp "$conf" "$stage/gamenative.conf"
chmod 644 "$stage/gamenative.conf"

built="$stage/asset.tzst"
# `--mtime` is what makes `--check` possible at all: without it the members carry the
# time they were copied, so two builds from identical inputs differ byte for byte and a
# stale asset can never be told from a fresh one.
( cd "$stage" && tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime='1970-01-01 00:00:00' -cf - alsa-lib gamenative.conf ) \
    | zstd -19 -q -o "$built"

if [ "$check" = "1" ]; then
    if cmp -s "$built" "$asset"; then
        echo "asset is current: $asset"
        exit 0
    fi
    echo "asset is STALE: $asset differs from what this tree builds" >&2
    exit 1
fi

mkdir -p "$(dirname "$asset")"
cp "$built" "$asset"
echo "wrote $asset"
md5sum "$asset"
echo
echo "members:"
tar -tvf "$asset" --zstd | sed 's/^/  /'
