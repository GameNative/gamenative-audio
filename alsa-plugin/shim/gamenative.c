/*
 * libasound_module_pcm_gamenative.so — the ALSA-facing half of the plugin.
 *
 * Everything here is mechanical glue between libasound's ioplug contract and the
 * Rust crate that does the work (../src/). Three things genuinely have to be C:
 *
 *   1. The exported symbol names. `snd_pcm_open` looks the plugin up by a name
 *      that `SND_PCM_PLUGIN_DEFINE_FUNC`/`SND_PCM_PLUGIN_SYMBOL` build out of
 *      identifier pasting (`_snd_pcm_<name>_open` plus a version tag). Writing
 *      those names by hand in Rust would hard-code something libasound derives
 *      from its own headers.
 *   2. `struct snd_pcm_ioplug`, which is a header-defined layout that libasound
 *      allocates, fills and frees. Describing it again in Rust could only add a
 *      way to get it wrong.
 *   3. `snd_pcm_channel_area_t` arithmetic, which is the same argument.
 *
 * Everything else — the ring, the format conversion, the engine, the state machine,
 * the error handling — lives in Rust. The functions below are one line each for a
 * reason: they translate a C call into a C-ABI call and nothing more.
 *
 * Build notes that are not optional:
 *   - `-DPIC` (the macro, not just -fPIC): without it `alsa/global.h` compiles the
 *     *static* dlsym branch and the plugin ends up referencing `snd_dlsym_start`,
 *     which libasound never exports → `dlopen` fails with
 *     "cannot locate symbol snd_dlsym_start".
 *   - libasound must end up in the shared object's DT_NEEDED, because Wine dlopens
 *     its unixlibs with RTLD_NOW and no RTLD_GLOBAL, so the plugin's own DT_NEEDED
 *     is the only scope its snd_* references can resolve from. (../build.sh links
 *     the imagefs libasound.so.2 directly for that reason.)
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <alsa/asoundlib.h>
#include <alsa/pcm_external.h>

#define ARRAY_SIZE(arr) (sizeof(arr) / sizeof((arr)[0]))

/* The Rust crate's C-ABI surface: src/pcm.rs. */
extern void *gnaudio_alsa_create(void);
extern void gnaudio_alsa_destroy(void *pcm);
extern int gnaudio_alsa_hw_params(void *pcm, int rate, int channels, int format,
                                  unsigned int period_frames, unsigned int buffer_frames);
extern void gnaudio_alsa_hw_free(void *pcm);
extern int gnaudio_alsa_prepare(void *pcm);
extern int gnaudio_alsa_start(void *pcm);
extern int gnaudio_alsa_stop(void *pcm);
extern int gnaudio_alsa_pause(void *pcm, int enable);
extern int gnaudio_alsa_drain(void *pcm);
extern long long gnaudio_alsa_transfer(void *pcm, const void *src, unsigned int frames);
extern unsigned long long gnaudio_alsa_position(void *pcm);
extern void gnaudio_alsa_note_depth(void *pcm, unsigned long long depth,
                                    unsigned long long buffer_size);
extern int gnaudio_alsa_is_open(void *pcm);

static int gamenative_start(snd_pcm_ioplug_t *io) {
    return gnaudio_alsa_start(io->private_data);
}

static int gamenative_stop(snd_pcm_ioplug_t *io) {
    return gnaudio_alsa_stop(io->private_data);
}

/*
 * The hardware pointer.
 *
 * **Monotone and unbounded, not reduced modulo the buffer size.** libasound keeps
 * `appl_ptr` in a space that runs up to the boundary (2^31) before wrapping, and
 * computes `hw_avail = appl_ptr - hw_ptr` against it; a pointer that wrapped at
 * `buffer_size` therefore went backwards relative to `appl_ptr` every 960 frames and
 * libasound stopped believing the device was consuming. Measured: writes throttled to
 * one period per poll timeout, 1239 frames/s where 48000 were expected, with no time
 * spent in this plugin's transfer path at all. Reducing modulo the boundary is
 * libasound's job, and the reference ioplug plugin on this device does the same.
 *
 * It advances on its own even when the app writes nothing, because the device is fed
 * silence when the ring is empty; a pointer that only moved when the app wrote would
 * stall the first time a game paused its audio thread.
 */
static snd_pcm_sframes_t gamenative_pointer(snd_pcm_ioplug_t *io) {
    static int debug = -1;
    static int last_second = -1;
    snd_pcm_sframes_t position = (snd_pcm_sframes_t)gnaudio_alsa_position(io->private_data);

    /* How far libasound believes the app has run ahead of the device. This is what its
     * own arithmetic is modulo, so a depth past the buffer size is a number it cannot
     * represent and every decision it makes from it is wrong. */
    if (io->appl_ptr > io->hw_ptr) {
        gnaudio_alsa_note_depth(io->private_data, (unsigned long long)(io->appl_ptr - io->hw_ptr),
                                (unsigned long long)io->buffer_size);
    }

    if (debug < 0) {
        debug = getenv("GNAUDIO_ALSA_DEBUG") != NULL;
    }
    if (debug) {
        int second = (int)(position / 48000);
        if (second != last_second) {
            last_second = second;
            fprintf(stderr,
                    "shim: pointer=%ld period=%lu buffer=%lu hw=%lu appl=%lu state=%d\n",
                    (long)position, (unsigned long)io->period_size, (unsigned long)io->buffer_size,
                    (unsigned long)io->hw_ptr, (unsigned long)io->appl_ptr, (int)io->state);
            fflush(stderr);
        }
    }
    return position;
}

/*
 * Takes the app's PCM. The channel areas describe an interleaved buffer, so the
 * first area's `first`/`step` locate the frame at `offset` — the same arithmetic
 * every ioplug plugin does.
 */
static snd_pcm_sframes_t gamenative_transfer(snd_pcm_ioplug_t *io,
                                            const snd_pcm_channel_area_t *areas,
                                            snd_pcm_uframes_t offset,
                                            snd_pcm_uframes_t size) {
    if (!areas || areas[0].addr == NULL) {
        return -EINVAL;
    }
    const char *data = (const char *)areas[0].addr + (areas[0].first + areas[0].step * offset) / 8;
    if (getenv("GNAUDIO_ALSA_DEBUG") != NULL) {
        fprintf(stderr, "shim: transfer size=%lu offset=%lu hw=%lu appl=%lu\n", (unsigned long)size,
                (unsigned long)offset, (unsigned long)io->hw_ptr, (unsigned long)io->appl_ptr);
        fflush(stderr);
    }
    long long taken = gnaudio_alsa_transfer(io->private_data, data, (unsigned int)size);
    if (taken < 0) {
        return (snd_pcm_sframes_t)taken;
    }
    return (snd_pcm_sframes_t)taken;
}

static int gamenative_close(snd_pcm_ioplug_t *io) {
    /* libasound frees the `snd_pcm_ioplug_t` itself, so only the Rust handle is
     * released here. */
    if (io->private_data) {
        gnaudio_alsa_destroy(io->private_data);
        io->private_data = NULL;
    }
    return 0;
}

/*
 * The parameters libasound has already agreed on are read out of `io` rather than
 * out of `params`: ioplug fills `io->format`, `io->rate`, `io->channels`,
 * `io->period_size` and `io->buffer_size` before it calls this, and those are the
 * numbers the app will actually write with.
 */
static int gamenative_hw_params(snd_pcm_ioplug_t *io, snd_pcm_hw_params_t *params) {
    (void)params;
    return gnaudio_alsa_hw_params(io->private_data,
                                  (int)io->rate,
                                  (int)io->channels,
                                  (int)io->format,
                                  (unsigned int)io->period_size,
                                  (unsigned int)io->buffer_size);
}

static int gamenative_hw_free(snd_pcm_ioplug_t *io) {
    gnaudio_alsa_hw_free(io->private_data);
    return 0;
}

static int gamenative_prepare(snd_pcm_ioplug_t *io) {
    return gnaudio_alsa_prepare(io->private_data);
}

static int gamenative_drain(snd_pcm_ioplug_t *io) {
    return gnaudio_alsa_drain(io->private_data);
}

static int gamenative_pause(snd_pcm_ioplug_t *io, int enable) {
    return gnaudio_alsa_pause(io->private_data, enable);
}

/*
 * Deliberately no `.delay` callback.
 *
 * libasound derives the delay from `appl_ptr - hw_ptr` when the plugin does not
 * provide one, and that is the honest figure here: `hw_ptr` is frames the device has
 * actually consumed, so the difference is what the app has written and not yet heard.
 *
 * Adding a callback that also counted what the *device's* ring holds was tried and was
 * wrong twice over — that counter reads frames handed to AAudio rather than frames
 * played, and it can exceed the ALSA buffer size outright (3264 seen against a
 * 960-frame buffer). libasound then believes the buffer is permanently full and
 * throttles every write: a 12000-frame write took 9.85 s where 0.25 s of audio was
 * expected, with no time at all spent in this plugin's own transfer path.
 */

static const snd_pcm_ioplug_callback_t gamenative_callback = {
    .start = gamenative_start,
    .stop = gamenative_stop,
    .pointer = gamenative_pointer,
    .transfer = gamenative_transfer,
    .close = gamenative_close,
    .hw_params = gamenative_hw_params,
    .hw_free = gamenative_hw_free,
    .prepare = gamenative_prepare,
    .drain = gamenative_drain,
    .pause = gamenative_pause,
};

/*
 * What this PCM will accept.
 *
 * The rate is pinned to 48000 rather than offered as a range: AAudio's own rate is
 * the one the device is running at, and letting the app pick 44100 here would mean
 * either a resample we did not ask for or a device that silently runs at a rate the
 * app did not choose. The `plug` wrapper in gamenative.conf is what converts an
 * app's 44100 to this — in software, once, where it can be seen.
 */
static int gamenative_set_hw_constraint(snd_pcm_ioplug_t *io) {
    static const unsigned int access_list[] = {SND_PCM_ACCESS_RW_INTERLEAVED};
    static const unsigned int format_list[] = {SND_PCM_FORMAT_S16_LE, SND_PCM_FORMAT_S32_LE,
                                               SND_PCM_FORMAT_FLOAT_LE};
    int err;

    err = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_ACCESS, ARRAY_SIZE(access_list),
                                        access_list);
    if (err < 0) {
        return err;
    }
    err = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_FORMAT, ARRAY_SIZE(format_list),
                                        format_list);
    if (err < 0) {
        return err;
    }
    err = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_CHANNELS, 1, 2);
    if (err < 0) {
        return err;
    }
    err = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_RATE, 48000, 48000);
    if (err < 0) {
        return err;
    }
    /* A floor here does not enlarge the buffer. Winealsa asks for a period of the app's own
     * size and four of them; refusing that period does not buy more room, it makes libasound
     * fall back to `set_buffer_size_near`, which gives the same 1 920 frame buffer with half
     * the periods — measured, a 3 840 byte floor turned a 480 frame period into 960 with the
     * buffer unchanged. The room comes from what the position reports. */
    err = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_PERIOD_BYTES, 256, 64 * 1024);
    if (err < 0) {
        return err;
    }
    /* Two to eight periods, which is what libasound uses to size the buffer from the
     * period the caller asks for. Raising the floor here does not enlarge the buffer:
     * libasound answers by shrinking the period instead, which was measured — a 12-period
     * floor turned a 480 frame period into 160 and left the buffer at 1920. */
    err = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_PERIODS, 2, 8);
    if (err < 0) {
        return err;
    }
    return 0;
}

static int gamenative_create(snd_pcm_t **pcmp, const char *name, snd_pcm_stream_t stream,
                             int mode) {
    snd_pcm_ioplug_t *io;
    int err;

    if (stream != SND_PCM_STREAM_PLAYBACK) {
        return -ENOTSUP;
    }

    io = calloc(1, sizeof(*io));
    if (!io) {
        return -ENOMEM;
    }

    io->version = SND_PCM_IOPLUG_VERSION;
    io->name = "GameNative Audio";
    io->callback = &gamenative_callback;
    io->mmap_rw = 0;

    io->private_data = gnaudio_alsa_create();
    if (!io->private_data) {
        free(io);
        return -ENOMEM;
    }

    err = snd_pcm_ioplug_create(io, name, stream, mode);
    if (err < 0) {
        gnaudio_alsa_destroy(io->private_data);
        free(io);
        return err;
    }

    err = gamenative_set_hw_constraint(io);
    if (err < 0) {
        snd_pcm_ioplug_delete(io);
        return err;
    }

    *pcmp = io->pcm;
    return 0;
}

/*
 * The body of what `SND_PCM_PLUGIN_DEFINE_FUNC(gamenative)` would have declared,
 * minus the name. The exported name itself is defined in Rust (src/entry.rs), for a
 * reason worth stating: rustc links a `cdylib` with a version script that lists only
 * the symbols the Rust crate exports, which silently localises anything a companion
 * object file defines. Defining the entry point on the Rust side keeps it in .dynsym
 * and keeps the mangled name next to the comment that explains it.
 */
int gamenative_open_entry(snd_pcm_t **pcmp, const char *name, snd_config_t *root,
                          snd_config_t *conf, snd_pcm_stream_t stream, int mode) {
    snd_config_iterator_t i, next;

    (void)root;

    snd_config_for_each(i, next, conf) {
        snd_config_t *n = snd_config_iterator_entry(i);
        const char *id;

        if (snd_config_get_id(n, &id) < 0) {
            continue;
        }
        if (strcmp(id, "type") == 0 || strcmp(id, "hint") == 0) {
            continue;
        }
        return -EINVAL;
    }

    return gamenative_create(pcmp, name, stream, mode);
}
