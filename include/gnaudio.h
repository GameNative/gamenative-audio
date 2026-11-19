/*
 * gnaudio — a low-latency output engine for Android: the client owns its block
 * size, the render callback is the clock, and the device's answer is reported
 * rather than promised.
 *
 * Copyright (C) 2026 GameNative
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The callback below runs on the audio device's real-time thread. In it, and in
 * anything it calls: no allocation, no locks, no logging, no blocking calls. If a
 * block cannot be produced in time the device plays silence and the engine counts a
 * dropout.
 */

#ifndef GNAUDIO_H
#define GNAUDIO_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ------------------------------------------------------------------------- */
/* Results                                                                    */
/* ------------------------------------------------------------------------- */

#define GNAUDIO_RESULT_OK 0
#define GNAUDIO_RESULT_UNAVAILABLE 1
#define GNAUDIO_RESULT_UNSUPPORTED 2
#define GNAUDIO_RESULT_DISCONNECTED 3
#define GNAUDIO_RESULT_INVALID_STATE 4
#define GNAUDIO_RESULT_NO_MEMORY 5
#define GNAUDIO_RESULT_DEVICE 6
#define GNAUDIO_RESULT_NO_BACKEND 7
#define GNAUDIO_RESULT_INVALID_ARGUMENT 8

/* A stable description of a result code, for logging. Never null. */
const char *gnaudio_result_text(int result);

/* ------------------------------------------------------------------------- */
/* Configuration                                                              */
/* ------------------------------------------------------------------------- */

/* The device Android considers the default. */
#define GNAUDIO_DEFAULT_DEVICE 0

/* Where the stream sits relative to the mixer. Exclusive is a request: a device
 * that cannot honour it answers Shared, which gnaudio_granted reports. */
#define GNAUDIO_SHARING_EXCLUSIVE 0
#define GNAUDIO_SHARING_SHARED 1

/* How hard the device works to stay low latency. LowLatency for anything an
 * instrument is played through; PowerSaving for playback that does not care. */
#define GNAUDIO_PERFORMANCE_LOW_LATENCY 0
#define GNAUDIO_PERFORMANCE_POWER_SAVING 1

/* What the device is delivering. Fixed, not negotiated: the engine never
 * resamples or converts, and refuses an open it cannot serve exactly. */
#define GNAUDIO_FORMAT_FLOAT32 2

typedef struct gnaudio_config {
    /* Device id, or GNAUDIO_DEFAULT_DEVICE. */
    int32_t device_id;
    /* Channels the client fills. At least 1. */
    int32_t channels;
    /* 0 for the device's own rate. Any other value is honoured exactly or the
     * open fails: the engine does not resample. */
    int32_t sample_rate;
    /* Frames per render callback — the client's block size. At least 1.
     * Rounded up to a whole number of device bursts, never below one. */
    int32_t block_frames;
    /* Frames the device's own ring should hold, or 0 to let it choose. */
    int32_t device_buffer_frames;
    /* GNAUDIO_SHARING_* */
    int32_t sharing;
    /* GNAUDIO_PERFORMANCE_* */
    int32_t performance;
} gnaudio_config;

/* ------------------------------------------------------------------------- */
/* The render callback                                                        */
/* ------------------------------------------------------------------------- */

/* What the client needs to know to fill this block. `out` holds
 * `frames * channels` floats, channel-major: all of channel 0, then channel 1,
 * and so on. */
typedef struct gnaudio_render_info {
    /* Frames rendered by this stream before this block. Block-aligned, zero at
     * the first block after gnaudio_start, and continuous across a reopen. */
    uint64_t position;
    /* CLOCK_MONOTONIC timestamp taken as the block was handed out. */
    uint64_t time_ns;
    /* Frames in this block: the block size the device granted. */
    uint32_t frames;
    /* Channels in `out`. */
    uint32_t channels;
    /* The device's sample rate. */
    int32_t sample_rate;
    int32_t _reserved;
} gnaudio_render_info;

/* Fill one block. Runs on the device's real-time thread, once per block, with the
 * device waiting. `out` is pre-zeroed, so a callback with nothing to say can
 * return immediately. Only one thread calls this at a time.
 *
 * `user` is the pointer passed to gnaudio_open, and is only ever touched from this
 * thread. */
typedef void (*gnaudio_render_fn)(void *user, const gnaudio_render_info *info, float *out);

/* ------------------------------------------------------------------------- */
/* What the device gave                                                       */
/* ------------------------------------------------------------------------- */

typedef struct gnaudio_granted {
    int32_t device_id;
    int32_t sample_rate;
    /* Channels the device runs, which may exceed the client's: the extras are
     * silent. */
    int32_t device_channels;
    int32_t client_channels;
    /* Frames per device wakeup. */
    int32_t frames_per_burst;
    int32_t device_buffer_frames;
    /* The block size in force: the client's, rounded to whole bursts. Re-read
     * this instead of assuming what was asked for. */
    int32_t block_frames;
    /* GNAUDIO_SHARING_* as granted. Requesting Exclusive and finding Shared here
     * means the device could not honour it. */
    int32_t sharing;
    /* GNAUDIO_PERFORMANCE_* as granted. */
    int32_t performance;
    /* GNAUDIO_FORMAT_*. */
    int32_t format;
    /* Frames below the client that the device and its HAL add. */
    int32_t output_latency_frames;
} gnaudio_granted;

/* ------------------------------------------------------------------------- */
/* What the device has done                                                   */
/* ------------------------------------------------------------------------- */

typedef struct gnaudio_stats {
    /* Render callbacks served. */
    uint64_t callbacks;
    /* Frames handed to the device. */
    uint64_t frames;
    /* Sequence breaks the device counted itself. */
    uint32_t device_xruns;
    /* Blocks the client could not produce within one device period. */
    uint64_t underruns;
    /* Devices built after the first, i.e. recoveries from a disconnect. */
    uint64_t reopens;
} gnaudio_stats;

/* ------------------------------------------------------------------------- */
/* Events                                                                     */
/* ------------------------------------------------------------------------- */

/* The device went away. The engine stops consuming; call gnaudio_reopen once it
 * is back. */
#define GNAUDIO_EVENT_DEVICE_LOST (1 << 0)
/* The clock is not continuous from the previous block: after a reopen. */
#define GNAUDIO_EVENT_RESYNC (1 << 1)
/* At least one block missed its deadline. Rate-limited to one per second. */
#define GNAUDIO_EVENT_UNDERRUN (1 << 2)

/* ------------------------------------------------------------------------- */
/* Device lifetime                                                            */
/* ------------------------------------------------------------------------- */

/* Opens the device, ready to render through `render`. Returns a handle, or null on
 * failure with the reason in `result` when it is not null. Nothing is consumed
 * until gnaudio_start. */
void *gnaudio_open(const gnaudio_config *config, gnaudio_render_fn render, void *user,
                   int *result);

/* Begins playback from position zero. */
int gnaudio_start(void *engine);

/* Stops consuming, keeps the device, freezes the position, and discards whatever
 * the device was still holding so a resume cannot replay it. */
int gnaudio_pause(void *engine);

/* Continues from a pause, with no gap: the next block follows the last one
 * rendered. */
int gnaudio_resume(void *engine);

/* Stops the device. The position stays readable; the next start is a new
 * timeline from zero. */
int gnaudio_stop(void *engine);

/* Rebuilds the device after a disconnect, keeping the client and the position and
 * raising GNAUDIO_EVENT_RESYNC. GNAUDIO_RESULT_DISCONNECTED means it is not back
 * yet. */
int gnaudio_reopen(void *engine);

/* Closes the device and frees the handle. Safe on null. The handle must not be
 * used afterwards. */
void gnaudio_close(void *engine);

/* ------------------------------------------------------------------------- */
/* Reporting                                                                  */
/* ------------------------------------------------------------------------- */

/* What the device granted when it was opened. */
int gnaudio_granted(void *engine, gnaudio_granted *out);

/* Callbacks, frames, dropouts. Safe to call while the device is running. */
int gnaudio_stats(void *engine, gnaudio_stats *out);

/* Frames between a sample being handed to the device and it being audible, right
 * now: recomputed from the device's live figures rather than promised at open.
 * Negative when there is no device. */
int32_t gnaudio_latency_frames(void *engine);

/* Block-aligned position, or -1 when there is no device. */
int64_t gnaudio_sample_position(void *engine);

/* The device's own clock, when it can produce one — prefer it over the callback
 * cadence for drift-free sync. Returns GNAUDIO_RESULT_UNAVAILABLE when the device
 * has no timestamp; either out-parameter may be null. */
int gnaudio_timestamp(void *engine, int64_t *frame_position, int64_t *time_ns);

/* Drains pending GNAUDIO_EVENT_* bits, as an int. The only way a client learns a
 * device went away underneath it. */
int gnaudio_take_events(void *engine);

/* Turns the engine's logcat output (tag "gnaudio") on or off. On by default. */
void gnaudio_set_logging(int enabled);

#ifdef __cplusplus
}
#endif

#endif /* GNAUDIO_H */
