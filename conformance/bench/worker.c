/* Zero-overhead C benchmark worker for head-to-head liblsl vs labstream measurements.
 * Compiled twice with -O2: once against pinned C++ liblsl, once against labstream.
 * Uses dlsym() for optional labstream_* health symbols so the same source builds cleanly
 * against both libraries.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#include "lsl_c.h"

typedef int32_t (*fn_inlet_int)(lsl_inlet);
typedef int64_t (*fn_inlet_i64)(lsl_inlet);
typedef double (*fn_inlet_f64)(lsl_inlet);

static double cpu_seconds(void) {
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

static double mono_sec(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + ts.tv_nsec * 1e-9;
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

static int cmp_double(const void *a, const void *b) {
    double da = *(const double *)a;
    double db = *(const double *)b;
    return (da > db) - (da < db);
}

static double pct(const double *sorted, size_t n, double p) {
    if (n == 0) return 0.0;
    size_t idx = (size_t)(p * (double)(n - 1) + 0.5);
    if (idx >= n) idx = n - 1;
    return sorted[idx];
}

static int run_outlet(const char *name, int ch, double srate, int chunk, double secs) {
    lsl_streaminfo info = lsl_create_streaminfo(name, "EEG", ch, srate, cft_float32, "bench-src");
    lsl_outlet out = lsl_create_outlet(info, chunk, 360);
    if (!out) {
        fprintf(stderr, "failed to create outlet\n");
        return 1;
    }

    float *buf = (float *)calloc((size_t)(chunk * ch), sizeof(float));
    lsl_wait_for_consumers(out, 10.0);

    double wall0 = mono_sec();
    double cpu0 = cpu_seconds();
    uint64_t seq = 0;
    uint64_t total = (uint64_t)(srate * secs);

    while (seq < total) {
        int n = chunk;
        if (seq + (uint64_t)n > total) n = (int)(total - seq);
        for (int k = 0; k < n; k++) {
            double t = (double)(seq + (uint64_t)k) / srate;
            for (int c = 0; c < ch; c++) {
                double v = 250.0 + c * 2.5 + 24.0 * sin(6.28318530718 * 10.0 * t + c * 0.13);
                buf[k * ch + c] = (float)(floor(v * 20.0 + 0.5) * 0.05);
            }
        }
        double now_lsl = lsl_local_clock();
        lsl_push_chunk_ftp(out, buf, (unsigned long)(n * ch), now_lsl, 1);
        seq += (uint64_t)n;
        sleep_until(wall0 + (double)seq / srate);
    }

    double wall_dt = mono_sec() - wall0;
    double cpu_dt = cpu_seconds() - cpu0;
    double cpu_pct = wall_dt > 0.0 ? (cpu_dt / wall_dt) * 100.0 : 0.0;

    /* Linger briefly so the inlet drains the final chunk / backfill. */
    usleep(300000);

    printf("{\"role\":\"outlet\",\"pushed\":%llu,\"wall_s\":%.3f,\"cpu_pct\":%.2f,\"rss_mb\":%.1f}\n",
           (unsigned long long)seq, wall_dt, cpu_pct, rss_mb());
    fflush(stdout);

    free(buf);
    lsl_destroy_outlet(out);
    lsl_destroy_streaminfo(info);
    return 0;
}

static int run_inlet(const char *name, int ch, double secs, double warmup) {
    lsl_streaminfo infos[4];
    int found = 0;
    double t_res = mono_sec();
    while (found == 0 && mono_sec() - t_res < 12.0) {
        found = lsl_resolve_byprop(infos, 4, "name", name, 1, 1.0);
    }
    if (found <= 0) {
        fprintf(stderr, "inlet failed to resolve %s\n", name);
        return 1;
    }

    lsl_inlet in = lsl_create_inlet(infos[0], 360, 0, 1);
    int32_t ec = 0;
    lsl_open_stream(in, 5.0, &ec);
    if (ec != 0) {
        fprintf(stderr, "lsl_open_stream failed: %d\n", ec);
        return 1;
    }

    size_t cap = 262144;
    double *lats = (double *)malloc(cap * sizeof(double));
    size_t n_lat = 0;

    const int max_pull = 256;
    float *data = (float *)malloc((size_t)(max_pull * ch) * sizeof(float));
    double *tss = (double *)malloc((size_t)max_pull * sizeof(double));

    double wall0 = mono_sec();
    double cpu0 = cpu_seconds();
    uint64_t total_rx = 0;

    while (mono_sec() - wall0 < secs) {
        unsigned long elems = lsl_pull_chunk_f(in, data, tss, (unsigned long)(max_pull * ch), (unsigned long)max_pull, 0.05, &ec);
        unsigned long ns = elems / (unsigned long)ch;
        if (ns == 0) continue;
        double now_lsl = lsl_local_clock();
        double elapsed = mono_sec() - wall0;
        total_rx += ns;
        if (elapsed >= warmup) {
            /* Record end-of-chunk sample latency (the newest sample stamped at push time). */
            double lat_us = (now_lsl - tss[ns - 1]) * 1e6;
            if (lat_us < 0.0) lat_us = 0.0;
            if (n_lat < cap) {
                lats[n_lat++] = lat_us;
            }
        }
    }

    double wall_dt = mono_sec() - wall0;
    double cpu_dt = cpu_seconds() - cpu0;
    double cpu_pct = wall_dt > 0.0 ? (cpu_dt / wall_dt) * 100.0 : 0.0;

    qsort(lats, n_lat, sizeof(double), cmp_double);

    /* Query optional labstream health symbols via dlsym (NULL on C++ liblsl). */
    fn_inlet_int f_shm = (fn_inlet_int)dlsym(RTLD_DEFAULT, "labstream_inlet_on_shm");
    fn_inlet_int f_comp = (fn_inlet_int)dlsym(RTLD_DEFAULT, "labstream_inlet_compressed");
    fn_inlet_f64 f_ratio = (fn_inlet_f64)dlsym(RTLD_DEFAULT, "labstream_inlet_compression_ratio");
    fn_inlet_int f_enc = (fn_inlet_int)dlsym(RTLD_DEFAULT, "labstream_inlet_encrypted");
    fn_inlet_i64 f_lost = (fn_inlet_i64)dlsym(RTLD_DEFAULT, "labstream_samples_lost");
    fn_inlet_i64 f_resent = (fn_inlet_i64)dlsym(RTLD_DEFAULT, "labstream_samples_resent");

    int on_shm = f_shm ? f_shm(in) : 0;
    int compressed = f_comp ? f_comp(in) : 0;
    double comp_ratio = f_ratio ? f_ratio(in) : 1.0;
    if (isnan(comp_ratio) || comp_ratio <= 0.0) comp_ratio = 1.0;
    int encrypted = f_enc ? f_enc(in) : 0;
    long long lost = f_lost ? (long long)f_lost(in) : -1;
    long long resent = f_resent ? (long long)f_resent(in) : -1;

    printf("{\"role\":\"inlet\",\"received\":%llu,\"lat_n\":%zu,"
           "\"p50_us\":%.2f,\"p95_us\":%.2f,\"p99_us\":%.2f,\"max_us\":%.2f,"
           "\"cpu_pct\":%.2f,\"rss_mb\":%.1f,"
           "\"on_shm\":%d,\"compressed\":%d,\"comp_ratio\":%.4f,\"encrypted\":%d,"
           "\"lost\":%lld,\"resent\":%lld}\n",
           (unsigned long long)total_rx, n_lat,
           pct(lats, n_lat, 0.50), pct(lats, n_lat, 0.95),
           pct(lats, n_lat, 0.99), pct(lats, n_lat, 1.00),
           cpu_pct, rss_mb(),
           on_shm, compressed, comp_ratio, encrypted, lost, resent);
    fflush(stdout);

    free(lats);
    free(data);
    free(tss);
    lsl_destroy_inlet(in);
    lsl_destroy_streaminfo(infos[0]);
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 5) {
        fprintf(stderr, "usage: worker outlet <name> <ch> <srate> <chunk> <secs>\n"
                        "       worker inlet  <name> <ch> <secs> [warmup_s]\n");
        return 2;
    }
    if (strcmp(argv[1], "outlet") == 0 && argc >= 7) {
        return run_outlet(argv[2], atoi(argv[3]), atof(argv[4]), atoi(argv[5]), atof(argv[6]));
    }
    if (strcmp(argv[1], "inlet") == 0 && argc >= 5) {
        double warmup = argc >= 6 ? atof(argv[5]) : 0.8;
        return run_inlet(argv[2], atoi(argv[3]), atof(argv[4]), warmup);
    }
    fprintf(stderr, "invalid arguments\n");
    return 2;
}
