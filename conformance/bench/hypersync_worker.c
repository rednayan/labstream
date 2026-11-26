/* Bare-metal head-to-head C benchmark worker for N-device hyperscanning:
 *   Compiled twice (-O2):
 *     - hypersync_liblsl    linked against pinned C++ liblsl (nearest-sample pairing)
 *     - hypersync_labstream linked against labstream (SyncGroup 32-tap Blackman-sinc)
 *
 * Usage:
 *   hypersync_worker <engine:naive|sync> <n_devs> <ch_per_dev> <base_srate> <mode:uniform|mixed|dropout> <stim_hz> <secs>
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#include "lsl_c.h"

typedef void *labstream_sync_group;
typedef labstream_sync_group (*fn_sync_create)(
    const lsl_streaminfo *, uint32_t, double, uint32_t, int32_t, double);
typedef uint32_t (*fn_sync_total_ch)(labstream_sync_group);
typedef int32_t (*fn_sync_pull)(
    labstream_sync_group, double *, double *, uint64_t *, uint32_t, double);
typedef void (*fn_sync_destroy)(labstream_sync_group);

#define MAX_DEVS 16
#define TWO_PI 6.28318530717958647692

static double mono_sec(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + ts.tv_nsec * 1e-9;
}

static double cpu_sec(void) {
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    return (double)ru.ru_utime.tv_sec + ru.ru_utime.tv_usec * 1e-6 +
           (double)ru.ru_stime.tv_sec + ru.ru_stime.tv_usec * 1e-6;
}

static double rss_mb(void) {
    FILE *f = fopen("/proc/self/status", "r");
    if (!f) return 0.0;
    char line[256];
    double mb = 0.0;
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, "VmRSS:", 6) == 0) {
            long kb = 0;
            if (sscanf(line + 6, "%ld", &kb) == 1) mb = kb / 1024.0;
            break;
        }
    }
    fclose(f);
    return mb;
}

static void sleep_until(double target) {
    double now = mono_sec();
    if (target <= now) return;
    double d = target - now;
    struct timespec req;
    req.tv_sec = (time_t)d;
    req.tv_nsec = (long)((d - req.tv_sec) * 1e9);
    nanosleep(&req, NULL);
}

typedef struct {
    int idx;
    char name[64];
    int channels;
    double srate;
    double offset_sec;
    double stim_hz;
    double secs;
    double drop_start;
    double drop_len;
    lsl_streaminfo info;
    lsl_outlet outlet;
    volatile int ready;
    volatile int stop;
    volatile int *sync_started;
} DevCfg;

static void *dev_thread(void *arg) {
    DevCfg *d = (DevCfg *)arg;
    int chunk = (int)lrint(d->srate * 0.02);
    if (chunk < 1) chunk = 1;
    float *buf = (float *)calloc((size_t)(chunk * d->channels), sizeof(float));

    double wall0 = mono_sec();
    double sync_t0 = -1.0;
    double base_ts = 1000.0;
    uint64_t seq = 0;

    while (!d->stop && (mono_sec() - wall0) < d->secs + 15.0) {
        double now_w = mono_sec();
        if (sync_t0 < 0.0 && d->sync_started && *d->sync_started) {
            sync_t0 = now_w;
        }
        int dropped = (d->drop_start > 0.0 && sync_t0 >= 0.0 &&
                       (now_w - sync_t0) >= d->drop_start &&
                       (now_w - sync_t0) < (d->drop_start + d->drop_len));
        if (!dropped) {
            for (int k = 0; k < chunk; k++) {
                double t = ((double)(seq + (uint64_t)k) / d->srate) + d->offset_sec;
                float val = (float)sin(TWO_PI * d->stim_hz * t);
                for (int c = 0; c < d->channels; c++) {
                    buf[k * d->channels + c] = val;
                }
            }
            double end_ts = base_ts + ((double)(seq + (uint64_t)(chunk - 1)) / d->srate) + d->offset_sec;
            lsl_push_chunk_ftp(d->outlet, buf, (unsigned long)(chunk * d->channels), end_ts, 1);
        }
        seq += (uint64_t)chunk;
        sleep_until(wall0 + (double)seq / d->srate);
    }

    free(buf);
    return NULL;
}

static double phase_err_deg_from_rms(double rms_diff, double rms_ref) {
    if (rms_ref < 1e-9) return 0.0;
    double ratio = (rms_diff / rms_ref) * 0.5;
    if (ratio > 1.0) ratio = 1.0;
    if (ratio < 0.0) ratio = 0.0;
    return 2.0 * asin(ratio) * (180.0 / 3.14159265358979323846);
}

int main(int argc, char **argv) {
    if (argc < 8) {
        fprintf(stderr, "usage: hypersync_worker <naive|sync> <n_devs> <ch> <base_srate> <uniform|mixed|dropout> <stim_hz> <secs>\n");
        return 2;
    }
    int use_sync = strcmp(argv[1], "sync") == 0;
    int n_devs = atoi(argv[2]);
    int ch_per_dev = atoi(argv[3]);
    double base_srate = atof(argv[4]);
    const char *mode = argv[5];
    double stim_hz = atof(argv[6]);
    double secs = atof(argv[7]);
    if (n_devs < 2 || n_devs > MAX_DEVS) return 2;

    int is_mixed = strcmp(mode, "mixed") == 0;
    int is_dropout = strcmp(mode, "dropout") == 0;
    double out_rate = base_srate;

    DevCfg devs[MAX_DEVS];
    pthread_t ths[MAX_DEVS];
    volatile int sync_started = 0;
    memset(devs, 0, sizeof(devs));

    /* Worst-case half-sample asynchronous ADC offset across the N devices: [0, 0.5 / base_srate]. */
    double max_offset = 0.5 / base_srate;

    for (int i = 0; i < n_devs; i++) {
        devs[i].idx = i;
        devs[i].sync_started = &sync_started;
        snprintf(devs[i].name, sizeof(devs[i].name), "HS_%s_%s_%d_%d", argv[1], mode, n_devs, i);
        devs[i].channels = ch_per_dev;
        if (is_mixed) {
            /* Mix of 2x, 1x, and 0.5x base_srate (e.g. 1000 / 500 / 250 Hz -> 500 Hz) */
            if (i % 4 == 0 || i % 4 == 1) devs[i].srate = base_srate * 2.0;
            else if (i % 4 == 2) devs[i].srate = base_srate;
            else devs[i].srate = base_srate * 0.5;
        } else {
            devs[i].srate = base_srate;
        }
        devs[i].offset_sec = ((double)i / (double)(n_devs - 1)) * max_offset;
        devs[i].stim_hz = stim_hz;
        devs[i].secs = secs;
        if (is_dropout && i == 3) {
            devs[i].drop_start = 1.6;
            devs[i].drop_len = 1.0;
        }
        /* Create outlets sequentially so UDP/TCP port allocation never races across threads. */
        devs[i].info = lsl_create_streaminfo(devs[i].name, "EEG", devs[i].channels, devs[i].srate, cft_float32, devs[i].name);
        devs[i].outlet = lsl_create_outlet(devs[i].info, 0, 360);
        if (!devs[i].outlet) {
            fprintf(stderr, "failed to create outlet %d (%s)\n", i, devs[i].name);
            return 1;
        }
        devs[i].ready = 1;
    }
    for (int i = 0; i < n_devs; i++) {
        pthread_create(&ths[i], NULL, dev_thread, &devs[i]);
    }
    usleep(120000);

    lsl_streaminfo resolved[MAX_DEVS];
    memset(resolved, 0, sizeof(resolved));
    int resolved_cnt = 0;
    double t_res = mono_sec();
    while (resolved_cnt < n_devs && mono_sec() - t_res < 12.0) {
        lsl_streaminfo all[64];
        int n = lsl_resolve_all(all, 64, 0.8);
        for (int k = 0; k < n; k++) {
            const char *nm = lsl_get_name(all[k]);
            int matched = 0;
            for (int i = 0; i < n_devs; i++) {
                if (!resolved[i] && nm && strcmp(nm, devs[i].name) == 0) {
                    resolved[i] = all[k];
                    resolved_cnt++;
                    matched = 1;
                    break;
                }
            }
            if (!matched) lsl_destroy_streaminfo(all[k]);
        }
    }
    for (int i = 0; i < n_devs && resolved_cnt < n_devs; i++) {
        if (resolved[i]) continue;
        lsl_streaminfo tmp[2];
        if (lsl_resolve_byprop(tmp, 2, "name", devs[i].name, 1, 2.0) > 0) {
            resolved[i] = tmp[0];
            resolved_cnt++;
        }
    }
    if (resolved_cnt < n_devs) {
        fprintf(stderr, "failed to resolve all %d devices (got %d); missing:", n_devs, resolved_cnt);
        for (int i = 0; i < n_devs; i++) {
            if (!resolved[i]) fprintf(stderr, " %d(%s)", i, devs[i].name);
        }
        fprintf(stderr, "\n");
        return 1;
    }

    double sq_diff[MAX_DEVS] = {0};
    double sq_ref[MAX_DEVS] = {0};
    uint64_t clean_n[MAX_DEVS] = {0};
    uint64_t gap_dev_counts[MAX_DEVS] = {0};
    uint64_t total_rows = 0;
    uint32_t total_ch = (uint32_t)(n_devs * ch_per_dev);

    double wall0 = 0.0, cpu0 = 0.0, wall_dt = 0.0, cpu_dt = 0.0, rss = 0.0;

    if (use_sync) {
        fn_sync_create f_create = (fn_sync_create)dlsym(RTLD_DEFAULT, "labstream_sync_create");
        fn_sync_total_ch f_ch = (fn_sync_total_ch)dlsym(RTLD_DEFAULT, "labstream_sync_total_channels");
        fn_sync_pull f_pull = (fn_sync_pull)dlsym(RTLD_DEFAULT, "labstream_sync_pull");
        fn_sync_destroy f_destroy = (fn_sync_destroy)dlsym(RTLD_DEFAULT, "labstream_sync_destroy");
        if (!f_create || !f_pull) {
            fprintf(stderr, "labstream_sync_* symbols not found\n");
            return 1;
        }
        labstream_sync_group sg = f_create(resolved, (uint32_t)n_devs, out_rate, 16, 0, 5.0);
        if (!sg) {
            fprintf(stderr, "labstream_sync_create failed\n");
            return 1;
        }
        total_ch = f_ch(sg);
        const uint32_t batch_cap = 128;
        double *data = (double *)malloc((size_t)(batch_cap * total_ch) * sizeof(double));
        double *tss = (double *)malloc((size_t)batch_cap * sizeof(double));
        uint64_t *masks = (uint64_t *)malloc((size_t)batch_cap * sizeof(uint64_t));

        wall0 = mono_sec();
        cpu0 = cpu_sec();
        sync_started = 1;

        uint64_t settle = (uint64_t)(out_rate * 0.25);
        while (mono_sec() - wall0 < secs) {
            int32_t got = f_pull(sg, data, tss, masks, batch_cap, 0.05);
            if (got <= 0) continue;
            for (int32_t r = 0; r < got; r++) {
                total_rows++;
                uint64_t m = masks[r];
                for (int d = 0; d < n_devs; d++) {
                    if ((m & (1ULL << d)) != 0) gap_dev_counts[d]++;
                }
                if (total_rows <= settle) continue;
                if ((m & 1ULL) != 0) continue;
                double ref = data[r * total_ch + 0];
                if (!isfinite(ref)) continue;
                for (int d = 1; d < n_devs; d++) {
                    if ((m & (1ULL << d)) != 0) continue;
                    double v = data[r * total_ch + (uint32_t)(d * ch_per_dev)];
                    if (!isfinite(v)) continue;
                    double diff = ref - v;
                    sq_diff[d] += diff * diff;
                    sq_ref[d] += ref * ref;
                    clean_n[d]++;
                }
            }
        }
        wall_dt = mono_sec() - wall0;
        cpu_dt = cpu_sec() - cpu0;
        rss = rss_mb();
        f_destroy(sg);
        free(data);
        free(tss);
        free(masks);
    } else {
        /* Standard LSL nearest-sample pairing across N real lsl_inlets (C++ liblsl). */
        lsl_inlet inlets[MAX_DEVS];
        int32_t ec = 0;
        for (int i = 0; i < n_devs; i++) {
            inlets[i] = lsl_create_inlet(resolved[i], 360, 0, 1);
            lsl_open_stream(inlets[i], 5.0, &ec);
        }

        const int max_pull = 256;
        float *buf = (float *)malloc((size_t)(max_pull * ch_per_dev) * sizeof(float));
        double *tss = (double *)malloc((size_t)max_pull * sizeof(double));

        /* Store latest sample (ch0) per device as each inlet drains — standard nearest-sample pairing. */
        double latest_val[MAX_DEVS] = {0};
        int has_val[MAX_DEVS] = {0};

        wall0 = mono_sec();
        cpu0 = cpu_sec();
        sync_started = 1;

        while (mono_sec() - wall0 < secs) {
            /* Drain devices 1..N-1 non-blocking so we always hold their latest sample. */
            for (int d = 1; d < n_devs; d++) {
                unsigned long elems = lsl_pull_chunk_f(
                    inlets[d], buf, tss, (unsigned long)(max_pull * ch_per_dev), (unsigned long)max_pull, 0.0, &ec);
                unsigned long ns = elems / (unsigned long)ch_per_dev;
                if (ns > 0) {
                    latest_val[d] = (double)buf[(ns - 1) * (unsigned long)ch_per_dev];
                    has_val[d] = 1;
                }
            }
            /* Pull reference device 0 and pair against each other device's nearest available sample. */
            unsigned long elems0 = lsl_pull_chunk_f(
                inlets[0], buf, tss, (unsigned long)(max_pull * ch_per_dev), (unsigned long)max_pull, 0.01, &ec);
            unsigned long ns0 = elems0 / (unsigned long)ch_per_dev;
            if (ns0 == 0) continue;
            total_rows += ns0;
            if (mono_sec() - wall0 < 0.4) continue;
            double ref = (double)buf[(ns0 - 1) * (unsigned long)ch_per_dev];
            for (int d = 1; d < n_devs; d++) {
                if (!has_val[d]) continue;
                double diff = ref - latest_val[d];
                sq_diff[d] += diff * diff;
                sq_ref[d] += ref * ref;
                clean_n[d]++;
            }
        }
        wall_dt = mono_sec() - wall0;
        cpu_dt = cpu_sec() - cpu0;
        rss = rss_mb();
        for (int i = 0; i < n_devs; i++) lsl_destroy_inlet(inlets[i]);
        free(buf);
        free(tss);
    }

    double cpu_pct = wall_dt > 0.0 ? (cpu_dt / wall_dt) * 100.0 : 0.0;
    double worst_rms_pct = 0.0;
    double worst_deg = 0.0;
    for (int d = 1; d < n_devs; d++) {
        if (clean_n[d] == 0) continue;
        double rms_d = sqrt(sq_diff[d] / (double)clean_n[d]);
        double rms_r = sqrt(sq_ref[d] / (double)clean_n[d]);
        double rms_pct = rms_r > 1e-9 ? (rms_d / rms_r) * 100.0 : 0.0;
        double deg = phase_err_deg_from_rms(rms_d, rms_r);
        if (rms_pct > worst_rms_pct) worst_rms_pct = rms_pct;
        if (deg > worst_deg) worst_deg = deg;
    }

    for (int i = 0; i < n_devs; i++) devs[i].stop = 1;
    for (int i = 0; i < n_devs; i++) {
        lsl_destroy_streaminfo(resolved[i]);
        pthread_join(ths[i], NULL);
        lsl_destroy_outlet(devs[i].outlet);
        lsl_destroy_streaminfo(devs[i].info);
    }

    uint64_t healthy_gaps = 0;
    for (int d = 0; d < n_devs; d++) {
        if (!(is_dropout && d == 3)) healthy_gaps += gap_dev_counts[d];
    }

    printf("{\"engine\":\"%s\",\"mode\":\"%s\",\"devices\":%d,\"base_srate\":%.1f,"
           "\"total_channels\":%u,\"stim_hz\":%.1f,"
           "\"aligned_rows\":%llu,\"worst_deg\":%.4f,\"worst_rms_pct\":%.4f,"
           "\"drop_dev3_gaps\":%llu,\"healthy_dev_gaps\":%llu,"
           "\"cpu_pct\":%.2f,\"rss_mb\":%.1f}\n",
           argv[1], mode, n_devs, base_srate, total_ch, stim_hz,
           (unsigned long long)total_rows, worst_deg, worst_rms_pct,
           (unsigned long long)(n_devs > 3 ? gap_dev_counts[3] : 0),
           (unsigned long long)healthy_gaps,
           cpu_pct, rss);
    fflush(stdout);
    return 0;
}
