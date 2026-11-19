#!/usr/bin/env python3
"""Find where a captured PCM stream is damaged, and say what kind of damage it is.

Reads the raw interleaved `f32` that the plugin writes when a stream opens
(`gn-pcm.raw` beside the image), and reports discontinuities in terms of the signal
that surrounds them. A click is a jump the waveform cannot have made, which is a
question about the audio around it and not about an absolute level — a fixed threshold
sees nothing in a quiet passage and sees everything in a loud one.

    analyse_pcm.py gn-pcm.raw [--rate 48000] [--channels 2] [--json out.json]

Every event carries a frame index, so it can be located in time and compared with what a
listener said about the same moment.
"""

import argparse
import array
import json
import math
import statistics
import sys

# A run of exact zeros counts as a hole only past this: 0.5 ms at 48 kHz. Shorter runs
# are ordinary audio — a zero crossing, a fade — and longer ones usually are not.
GAP_MS = 0.5
# Below this a sample is silence in any format a device accepts.
QUIET = 1.0e-6
# A step has to be this large relative to the RMS around it to be a splice rather than a
# transient. A real transient spans several samples, so it moves only a little between
# any two of them.
STEP_RATIO = 0.4
# ...and at least this large, so nothing inaudible is reported.
STEP_FLOOR = 0.01


def read_channels(path, channels):
    raw = open(path, "rb").read()
    if len(raw) % 4:
        raw = raw[: len(raw) - (len(raw) % 4)]
    samples = array.array("f")
    samples.frombytes(raw)
    if sys.byteorder == "big":
        samples.byteswap()
    if channels > 1:
        return [samples[c::channels] for c in range(channels)]
    return [samples]


def rms(samples, start, end):
    start = max(0, start)
    end = min(len(samples), end)
    if end <= start:
        return 0.0
    total = 0.0
    for value in samples[start:end]:
        total += value * value
    return math.sqrt(total / (end - start))


def find_events(samples, rate, channel):
    window = max(1, int(rate * 0.002))
    min_gap = max(1, int(rate * GAP_MS / 1000.0))
    events = []
    run = 0
    run_start = 0

    for index in range(1, len(samples)):
        value = samples[index]
        if abs(value) < QUIET:
            if run == 0:
                run_start = index
            run += 1
            continue

        if run >= min_gap:
            before = rms(samples, run_start - window, run_start)
            after = rms(samples, index, index + window)
            # Silence inside a passage that was making sound is a hole. Silence at the
            # start of a track, or in a rest, is the music.
            # Silence that runs from the beginning of the capture is the stream starting
            # quiet, not a hole in it — a capture that begins before the music does would
            # otherwise report one enormous gap and say nothing about the audio.
            leading = run_start <= min_gap
            if not leading and (before > 1.0e-3 or after > 1.0e-3):
                events.append(
                    {
                        "kind": "gap",
                        "channel": channel,
                        "frame": run_start,
                        "ms": round(run_start * 1000.0 / rate, 2),
                        "length_ms": round(run * 1000.0 / rate, 3),
                        "before_rms": round(before, 6),
                        "after_rms": round(after, 6),
                    }
                )
        run = 0

        step = abs(value - samples[index - 1])
        if step > STEP_FLOOR:
            before = rms(samples, index - window, index)
            after = rms(samples, index, index + window)
            # Both sides have to be sounding: a step out of silence is the edge of a gap,
            # already reported as one, and not a splice.
            if min(before, after) > 1.0e-3 and step > STEP_RATIO * max(before, after):
                events.append(
                    {
                        "kind": "step",
                        "channel": channel,
                        "frame": index,
                        "ms": round(index * 1000.0 / rate, 2),
                        "size": round(step, 4),
                        "rms_around": round(max(before, after), 5),
                    }
                )
    return events


def describe(events, frames, rate):
    # Both channels are counted, so an event present in both is one event in the signal
    # rather than two — reported once, with the channels it was found on.
    merged = {}
    for event in events:
        key = (event["kind"], event["frame"] // 8)
        merged.setdefault(key, []).append(event)
    unique = []
    for group in merged.values():
        first = dict(group[0])
        first["channels"] = sorted({e["channel"] for e in group})
        unique.append(first)
    unique.sort(key=lambda e: e["frame"])
    events = unique

    gaps = [e for e in events if e["kind"] == "gap"]
    steps = [e for e in events if e["kind"] == "step"]
    print(f"frames: {frames} ({frames / rate:.2f} s)")
    print(f"gaps : {len(gaps)}")
    if gaps:
        lengths = [e["length_ms"] for e in gaps]
        print(
            f"       length ms: min {min(lengths):.2f}  median "
            f"{statistics.median(lengths):.2f}  max {max(lengths):.2f}  "
            f"total {sum(lengths):.1f} ms"
        )
        for event in gaps[:12]:
            print(
                f"       at {event['ms']:9.1f} ms  {event['length_ms']:8.2f} ms  "
                f"rms before {event['before_rms']:.4f} after {event['after_rms']:.4f}  "
                f"ch {event['channels']}"
            )
    print(f"steps: {len(steps)}")
    if steps:
        sizes = [e["size"] for e in steps]
        print(
            f"       size: min {min(sizes):.3f}  median {statistics.median(sizes):.3f}  "
            f"max {max(sizes):.3f}"
        )
        for event in steps[:12]:
            print(
                f"       at {event['ms']:9.1f} ms  step {event['size']:.3f}  "
                f"against rms {event['rms_around']:.4f}  ch {event['channels']}"
            )

    return events


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("path")
    parser.add_argument("--rate", type=int, default=48_000)
    parser.add_argument("--channels", type=int, default=2)
    parser.add_argument("--json")
    args = parser.parse_args()

    channels = read_channels(args.path, args.channels)
    frames = len(channels[0])
    events = []
    for number, samples in enumerate(channels):
        events.extend(find_events(samples, args.rate, number))

    events = describe(events, frames, args.rate)

    if len(channels) > 1:
        both = sum(1 for e in events if len(e["channels"]) > 1)
        print(
            f"{both} of the events are in both channels — those are in the signal, and a "
            "one-channel event is in that channel only"
        )

    if args.json:
        with open(args.json, "w") as handle:
            json.dump(
                {"frames": frames, "rate": args.rate, "events": events}, handle, indent=1
            )
        print(f"wrote {args.json}")


if __name__ == "__main__":
    main()
