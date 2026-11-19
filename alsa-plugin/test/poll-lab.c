/*
 * Two arms, one question: what does libasound's blocking write do while it waits for
 * room, when an ioplug does and does not provide a poll source of its own?
 *
 * Both arms build the same ioplug PCM (the plugin's shape: start/stop/pointer/transfer/
 * close/hw_params/hw_free/prepare/drain/pause), leave it with no poll callbacks in the
 * "bug" arm, and run with stdin at a pipe whose writer is gone — which is what a wine
 * process gets, and what makes poll() return POLLHUP immediately and forever.
 *
 * A consumer thread plays the part of the engine: every 10 ms it takes one period out of
 * the position and, in the "fix" arm, signals the eventfd that the poll callbacks
 * report on. The writer asks for four buffers' worth of frames and must finish.
 *
 * Measured on a host, which is where this belongs: it is a property of libasound's
 * ioplug contract, not of any device. The "bug" arm never returns (the write is still
 * spinning when it is killed, at ~98 % of a core); the "fix" arm completes the same
 * write in one device-buffer's worth of time at ~0 % CPU.
 *
 * Build: gcc -O2 -o poll-lab poll-lab.c -lasound -lpthread
 * Run:   ./poll-lab bug ; ./poll-lab fix
 */
#define _GNU_SOURCE
#include <alsa/asoundlib.h>
#include <alsa/pcm_external.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#define RATE 48000
#define PERIOD 480
#define PERIODS 4
#define BUFFER (PERIOD * PERIODS)

static volatile int consuming;
static long position;          /* frames the "device" has played */
static int wake_fd = -1;       /* the fix arm's wakeup source */
static int fixed;              /* which arm we are */

static double cpu_seconds(void)
{
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    return ru.ru_utime.tv_sec + ru.ru_stime.tv_sec +
           (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) / 1e6;
}

static double now_seconds(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void sleep_ms(long ms)
{
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

/* The engine's stand-in: it consumes one period every 10 ms, which is what frees room. */
static void *consumer(void *arg)
{
    (void)arg;
    while (consuming) {
        __atomic_fetch_add(&position, PERIOD, __ATOMIC_RELEASE);
        if (wake_fd >= 0) {
            uint64_t one = 1;
            ssize_t ignored = write(wake_fd, &one, sizeof one);
            (void)ignored;
        }
        sleep_ms(10);
    }
    return NULL;
}

static snd_pcm_sframes_t pointer_cb(snd_pcm_ioplug_t *io)
{
    (void)io;
    return __atomic_load_n(&position, __ATOMIC_ACQUIRE);
}

static snd_pcm_sframes_t transfer_cb(snd_pcm_ioplug_t *io,
                                     const snd_pcm_channel_area_t *areas,
                                     snd_pcm_uframes_t offset,
                                     snd_pcm_uframes_t size)
{
    (void)io; (void)areas; (void)offset;
    return (snd_pcm_sframes_t)size;
}

static int start_cb(snd_pcm_ioplug_t *io) { (void)io; return 0; }
static int stop_cb(snd_pcm_ioplug_t *io) { (void)io; return 0; }
static int prepare_cb(snd_pcm_ioplug_t *io) { (void)io; return 0; }
static int drain_cb(snd_pcm_ioplug_t *io) { (void)io; return 0; }
static int pause_cb(snd_pcm_ioplug_t *io, int enable) { (void)io; (void)enable; return 0; }
static int hw_params_cb(snd_pcm_ioplug_t *io, snd_pcm_hw_params_t *p) { (void)io; (void)p; return 0; }
static int hw_free_cb(snd_pcm_ioplug_t *io) { (void)io; return 0; }
static int close_cb(snd_pcm_ioplug_t *io) { (void)io; return 0; }

static int poll_count_cb(snd_pcm_ioplug_t *io) { (void)io; return wake_fd >= 0 ? 1 : 0; }

static int poll_fds_cb(snd_pcm_ioplug_t *io, struct pollfd *pfds, unsigned int space)
{
    (void)io;
    if (wake_fd < 0) return 0;
    if (space < 1) return -ENOMEM;
    pfds->fd = wake_fd;
    pfds->events = POLLIN;
    pfds->revents = 0;
    return 1;
}

static int poll_revents_cb(snd_pcm_ioplug_t *io, struct pollfd *pfds, unsigned int nfds,
                           unsigned short *revents)
{
    (void)io;
    *revents = 0;
    if (!pfds || nfds < 1) return 0;
    if (pfds->revents & POLLNVAL) { *revents = POLLERR; return 0; }
    uint64_t value = 0;
    if (read(wake_fd, &value, sizeof value) == (ssize_t)sizeof value && value > 0)
        *revents = POLLOUT;
    return 0;
}

int main(int argc, char **argv)
{
    const char *arm = argc > 1 ? argv[1] : "bug";
    fixed = !strcmp(arm, "fix");
    snd_pcm_ioplug_t *io = calloc(1, sizeof(*io));
    int err;

    /* What a wine process has: stdin is a pipe at end of file. */
    {
        int fds[2];
        if (pipe(fds) < 0) { perror("pipe"); return 1; }
        close(fds[1]);
        dup2(fds[0], 0);
    }

    if (fixed) {
        wake_fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
        if (wake_fd < 0) { perror("eventfd"); return 1; }
    }

    io->version = SND_PCM_IOPLUG_VERSION;
    io->name = "pollab";
    io->callback = &(snd_pcm_ioplug_callback_t){
        .start = start_cb, .stop = stop_cb, .pointer = pointer_cb,
        .transfer = transfer_cb, .close = close_cb, .hw_params = hw_params_cb,
        .hw_free = hw_free_cb, .prepare = prepare_cb, .drain = drain_cb,
        .pause = pause_cb,
        .poll_descriptors_count = fixed ? poll_count_cb : NULL,
        .poll_descriptors = fixed ? poll_fds_cb : NULL,
        .poll_revents = fixed ? poll_revents_cb : NULL,
    };
    io->mmap_rw = 0;
    /* The bug arm's shape: an ioplug created from a zeroed struct polls descriptor 0.
     * The fix arm names -1, which the callbacks override. */
    io->poll_fd = fixed ? -1 : 0;

    err = snd_pcm_ioplug_create(io, "pollab", SND_PCM_STREAM_PLAYBACK, 0);
    if (err < 0) { fprintf(stderr, "create: %s\n", snd_strerror(err)); return 1; }

    static const unsigned int access_list[] = {SND_PCM_ACCESS_RW_INTERLEAVED};
    static const unsigned int format_list[] = {SND_PCM_FORMAT_S16_LE};
    snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_ACCESS, 1, access_list);
    snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_FORMAT, 1, format_list);
    snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_CHANNELS, 2, 2);
    snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_RATE, RATE, RATE);
    snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_PERIOD_BYTES, PERIOD * 4, PERIOD * 4);
    snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_PERIODS, PERIODS, PERIODS);

    snd_pcm_t *pcm = io->pcm;
    snd_pcm_hw_params_t *params;
    snd_pcm_hw_params_alloca(&params);
    snd_pcm_hw_params_any(pcm, params);
    snd_pcm_hw_params_set_access(pcm, params, SND_PCM_ACCESS_RW_INTERLEAVED);
    snd_pcm_hw_params_set_format(pcm, params, SND_PCM_FORMAT_S16_LE);
    snd_pcm_hw_params_set_channels(pcm, params, 2);
    snd_pcm_hw_params_set_rate(pcm, params, RATE, 0);
    snd_pcm_hw_params_set_buffer_size(pcm, params, BUFFER);
    snd_pcm_hw_params_set_period_size(pcm, params, PERIOD, 0);
    if ((err = snd_pcm_hw_params(pcm, params)) < 0) {
        fprintf(stderr, "hw_params: %s\n", snd_strerror(err));
        return 1;
    }
    snd_pcm_prepare(pcm);
    snd_pcm_start(pcm);

    consuming = 1;
    pthread_t thread;
    pthread_create(&thread, NULL, consumer, NULL);

    /* Four buffers: three of them have to wait for the device to make room. */
    size_t frames = (size_t)BUFFER * 4;
    int16_t *buffer = calloc(frames * 2, sizeof(int16_t));
    double cpu0 = cpu_seconds(), t0 = now_seconds();
    snd_pcm_sframes_t written = snd_pcm_writei(pcm, buffer, frames);
    double wall = now_seconds() - t0, cpu = cpu_seconds() - cpu0;

    consuming = 0;
    pthread_join(thread, NULL);

    printf("%-4s writei(%zu) -> %ld  wall %6.3f s  cpu %6.3f s  (%.0f%% of a core)\n",
           arm, frames, (long)written, wall, cpu, wall > 0 ? 100.0 * cpu / wall : 0.0);
    return 0;
}
