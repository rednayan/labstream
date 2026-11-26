/* Bare-metal C benchmark worker for N-device hyperscanning (2, 4, 8+ devices).
 *
 * Publishes N independent asynchronous LSL outlets (with distinct fractional ADC
 * offsets and optional heterogeneous rates / mid-run dropout on device #3), then
 * measures:
 *   1. Standard LSL nearest-sample pairing worst-pair phase & RMS error
 *   2. labstream SyncGroup (labstream_sync_create / labstream_sync_pull)
 *      worst-pair phase & RMS error, gap_mask accuracy, CPU %, and RSS MB.
 */
#define _GNU_SOURCE
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
extern labstream_sync_group labstream_sync_create(
    const lsl_streaminfo *infos, uint32_t count, double output_rate,
    uint32_t half_width, int32_t auto_postprocess, double timeout);
extern uint32_t labstream_sync_total_channels(labstream_sync_group sync);
extern int32_t labstream_sync_pull(
    labstream_sync_group sync, double *data_out, double *ts_out,
    uint64_t *gap_mask_out, uint32_t max_samples, double timeout);
extern void labstream_sync_destroy(labstream_sync_group sync);

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
    double drop_start; /* > 0 to drop this device mid-run */
    double drop_len;
    lsl_streaminfo info;
    lsl_outlet outlet;
    volatile int ready;
    volatile int stop;
    volatile int *sync_started;
} DevCfg;

static void *dev_thread(void *arg) {
    DevCfg *d = (DevCfg *)arg;
    d->info = lsl_create_streaminfo(d->name, "EEG", d->channels, d->srate, cft_float32, d->name);
    d->outlet = lsl_create_outlet(d->info, 0, 360);
    d->ready = 1;

    int chunk = (int)lrint(d->srate * 0.02);
    if (chunk < 1) chunk = 1;
    float *buf = (float *)calloc((size_t)(chunk * d->channels), sizeof(float));

    double wall0 = mono_sec();
    double sync_t0 = -1.0;
    double base_ts = 1000.0;
    uint64_t seq = 0;

    while (!d->stop && (mono_sec() - wall0) < d->secs + 12.0) {
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
    lsl_destroy_outlet(d->outlet);
    lsl_destroy_streaminfo(d->info);
    return NULL;
}

static double phase_err_deg_from_rms(double rms_diff, double rms_ref) {
    /* For two unit-amplitude sine waves shifted by theta: rms(sin(wt) - sin(wt+theta)) / rms(sin(wt)) = 2*sin(theta/2). */
    if (rms_ref < 1e-9) return 0.0;
    double ratio = (rms_diff / rms_ref) * 0.5;
    if (ratio > 1.0) ratio = 1.0;
    if (ratio < 0.0) ratio = 0.0;
    return 2.0 * asin(ratio) * (180.0 / 3.14159265358979323846);
}

int main(int argc, char **argv) {
    /* Usage: hypersync_worker <n_devs> <channels_per_dev> <mode:uniform|mixed|dropout> <stim_hz> <secs> */
    if (argc < 6) {
        fprintf(stderr, "usage: hypersync_worker <n_devs> <ch_per_dev> <uniform|mixed|dropout> <stim_hz> <secs>\n");
        return 2;
    }
    int n_devs = atoi(argv[1]);
    int ch_per_dev = atoi(argv[2]);
    const char *mode = argv[3];
    double stim_hz = atof(argv[4]);
    double secs = atof(argv[5]);
    if (n_devs < 2 || n_devs > MAX_DEVS) return 2;

    int is_mixed = strcmp(mode, "mixed") == 0;
    int is_dropout = strcmp(mode, "dropout") == 0;
    double out_rate = 250.0;

    DevCfg devs[MAX_DEVS];
    pthread_t ths[MAX_DEVS];
    volatile int sync_started = 0;
    memset(devs, 0, sizeof(devs));

    for (int i = 0; i < n_devs; i++) {
        devs[i].idx = i;
        devs[i].sync_started = &sync_started;
        snprintf(devs[i].name, sizeof(devs[i].name), "HS_%s_%d_%d", mode, n_devs, i);
        devs[i].channels = ch_per_dev;
        if (is_mixed) {
            /* Mix of 500 Hz (EEG), 250 Hz (ECG), and 120 Hz (GSR/Eye) */
            if (i % 4 == 0 || i % 4 == 1) devs[i].srate = 500.0;
            else if (i % 4 == 2) devs[i].srate = 250.0;
            else devs[i].srate = 120.0;
        } else {
            devs[i].srate = 250.0;
        }
        /* Spread fractional offsets across [0, T_s/2] (worst-case 2.0 ms at 250 SPS). */
        devs[i].offset_sec = ((double)i / (double)(n_devs - 1)) * 0.002;
        devs[i].stim_hz = stim_hz;
        devs[i].secs = secs;
        if (is_dropout && i == 3) {
            devs[i].drop_start = 1.6;
            devs[i].drop_len = 1.0;
        }
        pthread_create(&ths[i], NULL, dev_thread, &devs[i]);
    }

    for (int i = 0; i < n_devs; i++) {
        while (!devs[i].ready) usleep(5000);
    }
    usleep(100000);

    /* Resolve all N streams */
    lsl_streaminfo resolved[MAX_DEVS];
    for (int i = 0; i < n_devs; i++) {
        lsl_streaminfo tmp[2];
        int found = 0;
        double t0 = mono_sec();
        while (found <= 0 && mono_sec() - t0 < 8.0) {
            found = lsl_resolve_byprop(tmp, 2, "name", devs[i].name, 1, 0.5);
        }
        if (found <= 0) {
            fprintf(stderr, "failed to resolve %s\n", devs[i].name);
            return 1;
        }
        resolved[i] = tmp[0];
    }

    /* Theoretical worst-case standard LSL nearest-sample pairing error across the N devices:
     * max offset between device 0 (0.0 ms) and device N-1 (2.0 ms) -> phase = 360 * stim_hz * 0.002. */
    double naive_worst_deg = fmod(360.0 * stim_hz * 0.002, 360.0);
    if (naive_worst_deg > 180.0) naive_worst_deg = 360.0 - naive_worst_deg;
    double naive_worst_rms_pct = 200.0 * sin((naive_worst_deg * 3.14159265358979323846 / 180.0) * 0.5);

    /* Create SyncGroup across all N devices (auto_postprocess=0 -> same monotonic clock, pure sinc aligner) */
    labstream_sync_group sg = labstream_sync_create(resolved, (uint32_t)n_devs, out_rate, 16, 0, 5.0);
    if (!sg) {
        fprintf(stderr, "labstream_sync_create failed\n");
        return 1;
    }
    uint32_t total_ch = labstream_sync_total_channels(sg);

    const uint32_t batch_cap = 64;
    double *data = (double *)malloc((size_t)(batch_cap * total_ch) * sizeof(double));
    double *tss = (double *)malloc((size_t)batch_cap * sizeof(double));
    uint64_t *masks = (uint64_t *)malloc((size_t)batch_cap * sizeof(uint64_t));

    double sq_diff[MAX_DEVS] = {0};
    double sq_ref[MAX_DEVS] = {0};
    uint64_t clean_n[MAX_DEVS] = {0};
    uint64_t gap_dev_counts[MAX_DEVS] = {0};
    uint64_t total_rows = 0;

    double wall0 = mono_sec();
    double cpu0 = cpu_sec();
    sync_started = 1;

    while (mono_sec() - wall0 < secs) {
        int32_t got = labstream_sync_pull(sg, data, tss, masks, batch_cap, 0.05);
        if (got <= 0) continue;
        for (int32_t r = 0; r < got; r++) {
            total_rows++;
            uint64_t m = masks[r];
            for (int d = 0; d < n_devs; d++) {
                if ((m & (1ULL << d)) != 0) gap_dev_counts[d]++;
            }
            /* Skip first 50 aligned rows (0.2 s filter settling) */
            if (total_rows <= 50) continue;
            if ((m & 1ULL) != 0) continue;
            double ref = data[r * total_ch + 0]; /* device 0 ch 0 */
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

    double wall_dt = mono_sec() - wall0;
    double cpu_dt = cpu_sec() - cpu0;
    double cpu_pct = wall_dt > 0.0 ? (cpu_dt / wall_dt) * 100.0 : 0.0;
    double rss = rss_mb();

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
    labstream_sync_destroy(sg);
    for (int i = 0; i < n_devs; i++) {
        lsl_destroy_streaminfo(resolved[i]);
        pthread_join(ths[i], NULL);
    }
    free(data);
    free(tss);
    free(masks);

    uint64_t healthy_gaps = 0;
    for (int d = 0; d < n_devs; d++) {
        if (!(is_dropout && d == 3)) healthy_gaps += gap_dev_counts[d];
    }

    printf("{\"mode\":\"%s\",\"devices\":%d,\"total_channels\":%u,\"stim_hz\":%.1f,"
           "\"aligned_rows\":%llu,\"rows_per_sec\":%.1f,"
           "\"naive_worst_deg\":%.2f,\"naive_worst_rms_pct\":%.2f,"
           "\"synced_worst_deg\":%.4f,\"synced_worst_rms_pct\":%.4f,"
           "\"drop_dev3_gaps\":%llu,\"healthy_dev_gaps\":%llu,"
           "\"cpu_pct\":%.2f,\"rss_mb\":%.1f}\n",
           mode, n_devs, total_ch, stim_hz,
           (unsigned long long)total_rows, wall_dt > 0.0 ? (double)total_rows / wall_dt : 0.0,
           naive_worst_deg, naive_worst_rms_pct,
           worst_deg, worst_rms_pct,
           (unsigned long long)(n_devs > 3 ? gap_dev_counts[3] : 0),
           (unsigned long long)healthy_gaps,
           cpu_pct, rss);
    fflush(stdout);
    return 0;
}
