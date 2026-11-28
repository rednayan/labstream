/* Generic live hardware LSL validation worker.
 *
 * Connects labstream to an active external LSL stream, measures real interop,
 * arrival timing, timestamp jitter, comp/1 wire compression on live samples,
 * and SyncGroup alignment. All stream names and host metadata are omitted from
 * output so reports remain completely vendor-agnostic.
 *
 * Usage:
 *   live_eeg_worker <stream_type|ALL> <secs>
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

typedef void *labstream_sync_group;
typedef labstream_sync_group (*fn_sync_create)(
    const lsl_streaminfo *, uint32_t, double, uint32_t, int32_t, double);
typedef uint32_t (*fn_sync_total_ch)(labstream_sync_group);
typedef int32_t (*fn_sync_pull)(
    labstream_sync_group, double *, double *, uint64_t *, uint32_t, double);
typedef void (*fn_sync_destroy)(labstream_sync_group);
typedef int32_t (*fn_inlet_compressed)(lsl_inlet);
typedef double (*fn_inlet_comp_ratio)(lsl_inlet);
typedef int64_t (*fn_samples_lost)(lsl_inlet);

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

static int cmp_dbl(const void *a, const void *b) {
    double x = *(const double *)a;
    double y = *(const double *)b;
    return (x > y) - (x < y);
}

static double pct(const double *sorted, size_t n, double p) {
    if (n == 0) return 0.0;
    size_t idx = (size_t)(p * (double)(n - 1));
    if (idx >= n) idx = n - 1;
    return sorted[idx];
}

static const char *fmt_name(lsl_channel_format_t f) {
    switch (f) {
        case cft_float32: return "float32";
        case cft_double64: return "double64";
        case cft_string: return "string";
        case cft_int32: return "int32";
        case cft_int16: return "int16";
        case cft_int8: return "int8";
        case cft_int64: return "int64";
        default: return "unknown";
    }
}

int main(int argc, char **argv) {
    const char *want_type = argc >= 2 ? argv[1] : "EEG";
    double secs = argc >= 3 ? atof(argv[2]) : 15.0;
    if (secs < 2.0) secs = 2.0;

    /* Discover active external LSL streams on the local machine or LAN. */
    lsl_streaminfo found[32];
    int n_found = lsl_resolve_all(found, 32, 3.0);
    if (n_found <= 0) {
        fprintf(stderr, "no active LSL streams found on the network\n");
        return 1;
    }

    int primary_idx = -1;
    int secondary_idx = -1;
    double best_sr = -1.0;
    for (int i = 0; i < n_found; i++) {
        const char *tp = lsl_get_type(found[i]);
        double sr = lsl_get_nominal_srate(found[i]);
        lsl_channel_format_t fmt = lsl_get_channel_format(found[i]);
        if (fmt == cft_string || sr <= 0.0) continue;
        int match = (strcmp(want_type, "ALL") == 0) ||
                    (tp && strcasecmp(tp, want_type) == 0);
        if (match && sr > best_sr) {
            best_sr = sr;
            primary_idx = i;
        }
    }
    if (primary_idx < 0) {
        /* Fall back to the highest-rate regular numeric stream (e.g. ExG/EEG). */
        best_sr = -1.0;
        for (int i = 0; i < n_found; i++) {
            double sr = lsl_get_nominal_srate(found[i]);
            if (sr > best_sr && lsl_get_channel_format(found[i]) != cft_string) {
                best_sr = sr;
                primary_idx = i;
            }
        }
    }
    for (int i = 0; i < n_found; i++) {
        if (i == primary_idx) continue;
        if (lsl_get_nominal_srate(found[i]) > 0.0 &&
            lsl_get_channel_format(found[i]) != cft_string) {
            secondary_idx = i;
            break;
        }
    }
    if (primary_idx < 0) {
        fprintf(stderr, "no numeric regular-rate LSL stream found\n");
        for (int i = 0; i < n_found; i++) lsl_destroy_streaminfo(found[i]);
        return 1;
    }

    lsl_streaminfo primary = found[primary_idx];
    int ch = lsl_get_channel_count(primary);
    double srate = lsl_get_nominal_srate(primary);
    lsl_channel_format_t fmt = lsl_get_channel_format(primary);
    const char *ptype = lsl_get_type(primary);
    if (!ptype) ptype = "unknown";

    /* Phase 1: connect labstream inlet to the live hardware stream and measure
     * arrival timing, timestamp step jitter, and comp/1 ratio via a local
     * loopback relay outlet. */
    int32_t ec = 0;
    lsl_inlet hw_in = lsl_create_inlet(primary, 360, 0, 1);
    lsl_open_stream(hw_in, 8.0, &ec);
    if (ec != 0) {
        fprintf(stderr, "failed to open live stream (ec=%d)\n", ec);
        return 1;
    }

    /* Local comp/1 relay so we measure real compression on live EEG data. */
    char relay_name[64];
    snprintf(relay_name, sizeof(relay_name), "ls_comp_probe_%d", (int)getpid());
    lsl_streaminfo r_info = lsl_create_streaminfo(relay_name, "CompProbe", ch, srate, cft_float32, relay_name);
    lsl_outlet r_out = lsl_create_outlet(r_info, 0, 360);
    lsl_streaminfo r_res[2];
    lsl_inlet r_in = NULL;
    if (r_out && lsl_resolve_byprop(r_res, 2, "name", relay_name, 1, 3.0) > 0) {
        r_in = lsl_create_inlet(r_res[0], 360, 0, 1);
        lsl_open_stream(r_in, 5.0, &ec);
        lsl_destroy_streaminfo(r_res[0]);
    }

    const size_t max_chunk = 2048;
    float *fbuf = (float *)malloc(max_chunk * (size_t)ch * sizeof(float));
    double *tbuf = (double *)malloc(max_chunk * sizeof(double));
    float *rbuf = (float *)malloc(max_chunk * (size_t)ch * sizeof(float));
    double *rtbuf = (double *)malloc(max_chunk * sizeof(double));

    size_t cap_arr = 65536;
    double *chunk_dt_ms = (double *)malloc(cap_arr * sizeof(double));
    size_t n_chunk_dt = 0;
    size_t cap_step = 262144;
    double *step_err_ms = (double *)malloc(cap_step * sizeof(double));
    size_t n_step_err = 0;

    uint64_t total_samples = 0;
    uint64_t total_chunks = 0;
    uint64_t finite_vals = 0;
    uint64_t total_vals = 0;
    uint64_t ts_gaps = 0;
    double sum_sq = 0.0;
    double min_v = 1e30, max_v = -1e30;
    double prev_chunk_w = -1.0;
    double prev_ts = -1.0;
    double first_ts = -1.0, last_ts = -1.0;
    double expected_dt = 1.0 / srate;

    /* Flush any pre-buffered samples before timing starts. */
    for (int k = 0; k < 5; k++) {
        lsl_pull_chunk_f(hw_in, fbuf, tbuf, (unsigned long)(max_chunk * (size_t)ch), (unsigned long)max_chunk, 0.02, &ec);
    }

    double t0 = mono_sec();
    double cpu0 = cpu_sec();
    while (mono_sec() - t0 < secs) {
        unsigned long elems = lsl_pull_chunk_f(
            hw_in, fbuf, tbuf, (unsigned long)(max_chunk * (size_t)ch), (unsigned long)max_chunk, 0.1, &ec);
        unsigned long ns = elems / (unsigned long)ch;
        if (ns == 0) continue;
        double now_w = mono_sec();
        total_chunks++;
        total_samples += ns;
        if (prev_chunk_w >= 0.0 && n_chunk_dt < cap_arr) {
            chunk_dt_ms[n_chunk_dt++] = (now_w - prev_chunk_w) * 1000.0;
        }
        prev_chunk_w = now_w;

        for (unsigned long s = 0; s < ns; s++) {
            double ts = tbuf[s];
            if (first_ts < 0.0) first_ts = ts;
            last_ts = ts;
            if (prev_ts >= 0.0) {
                double dt = ts - prev_ts;
                if (dt > expected_dt * 2.5) ts_gaps++;
                if (n_step_err < cap_step) {
                    step_err_ms[n_step_err++] = fabs(dt - expected_dt) * 1000.0;
                }
            }
            prev_ts = ts;
            for (int c = 0; c < ch; c++) {
                double v = (double)fbuf[s * (unsigned long)ch + (unsigned long)c];
                total_vals++;
                if (isfinite(v)) {
                    finite_vals++;
                    sum_sq += v * v;
                    if (v < min_v) min_v = v;
                    if (v > max_v) max_v = v;
                }
            }
        }

        /* Push real EEG chunk into comp/1 probe relay and drain it. */
        if (r_out && r_in) {
            lsl_push_chunk_ftnp(r_out, fbuf, elems, tbuf, 1);
            lsl_pull_chunk_f(r_in, rbuf, rtbuf, (unsigned long)(max_chunk * (size_t)ch), (unsigned long)max_chunk, 0.0, &ec);
        }
    }
    double wall_dt = mono_sec() - t0;
    double cpu_dt = cpu_sec() - cpu0;
    double p1_cpu = wall_dt > 0.0 ? (cpu_dt / wall_dt) * 100.0 : 0.0;

    /* Final drain of comp/1 relay inlet. */
    if (r_in) {
        usleep(50000);
        lsl_pull_chunk_f(r_in, rbuf, rtbuf, (unsigned long)(max_chunk * (size_t)ch), (unsigned long)max_chunk, 0.05, &ec);
    }

    fn_inlet_compressed f_comp = (fn_inlet_compressed)dlsym(RTLD_DEFAULT, "labstream_inlet_compressed");
    fn_inlet_comp_ratio f_ratio = (fn_inlet_comp_ratio)dlsym(RTLD_DEFAULT, "labstream_inlet_compression_ratio");
    fn_samples_lost f_lost = (fn_samples_lost)dlsym(RTLD_DEFAULT, "labstream_samples_lost");

    int comp_active = (r_in && f_comp) ? f_comp(r_in) : 0;
    double comp_ratio = (r_in && f_ratio) ? f_ratio(r_in) : 1.0;
    int64_t samples_lost = f_lost ? f_lost(hw_in) : 0;

    if (r_in) lsl_destroy_inlet(r_in);
    if (r_out) lsl_destroy_outlet(r_out);
    lsl_destroy_streaminfo(r_info);
    lsl_destroy_inlet(hw_in);

    qsort(chunk_dt_ms, n_chunk_dt, sizeof(double), cmp_dbl);
    qsort(step_err_ms, n_step_err, sizeof(double), cmp_dbl);

    double eff_hz = (last_ts > first_ts && total_samples > 1)
                        ? (double)(total_samples - 1) / (last_ts - first_ts)
                        : (wall_dt > 0.0 ? (double)total_samples / wall_dt : 0.0);
    double rms_v = finite_vals > 0 ? sqrt(sum_sq / (double)finite_vals) : 0.0;

    /* Phase 2: SyncGroup alignment on live hardware stream(s). */
    uint64_t sync_rows = 0;
    uint64_t sync_hw_gaps = 0;
    double sync_cpu = 0.0;
    double sync_rss = rss_mb();
    int sync_streams = 0;
    int sync_real_pair = 0;

    fn_sync_create f_sg_create = (fn_sync_create)dlsym(RTLD_DEFAULT, "labstream_sync_create");
    fn_sync_total_ch f_sg_ch = (fn_sync_total_ch)dlsym(RTLD_DEFAULT, "labstream_sync_total_channels");
    fn_sync_pull f_sg_pull = (fn_sync_pull)dlsym(RTLD_DEFAULT, "labstream_sync_pull");
    fn_sync_destroy f_sg_destroy = (fn_sync_destroy)dlsym(RTLD_DEFAULT, "labstream_sync_destroy");

    if (f_sg_create && f_sg_pull) {
        lsl_streaminfo sg_infos[2];
        sg_infos[0] = primary;
        sync_streams = 1;
        if (secondary_idx >= 0) {
            sg_infos[1] = found[secondary_idx];
            sync_streams = 2;
            sync_real_pair = 1;
        }
        double sg_secs = secs < 8.0 ? secs : 8.0;
        /* Enable clocksync + dejitter + monotonize for real cross-PC wireless streams. */
        labstream_sync_group sg = f_sg_create(sg_infos, (uint32_t)sync_streams, srate, 16, 1, 5.0);
        if (sg) {
            uint32_t tch = f_sg_ch(sg);
            const uint32_t bcap = 128;
            double *sdata = (double *)malloc((size_t)(bcap * tch) * sizeof(double));
            double *stss = (double *)malloc((size_t)bcap * sizeof(double));
            uint64_t *smasks = (uint64_t *)malloc((size_t)bcap * sizeof(uint64_t));
            double st0 = mono_sec();
            double sc0 = cpu_sec();
            while (mono_sec() - st0 < sg_secs) {
                int32_t got = f_sg_pull(sg, sdata, stss, smasks, bcap, 0.1);
                if (got <= 0) continue;
                for (int32_t r = 0; r < got; r++) {
                    sync_rows++;
                    if ((smasks[r] & 1ULL) != 0) sync_hw_gaps++;
                }
            }
            double sw = mono_sec() - st0;
            double sc = cpu_sec() - sc0;
            sync_cpu = sw > 0.0 ? (sc / sw) * 100.0 : 0.0;
            sync_rss = rss_mb();
            f_sg_destroy(sg);
            free(sdata);
            free(stss);
            free(smasks);
        }
    }

    for (int i = 0; i < n_found; i++) lsl_destroy_streaminfo(found[i]);
    free(fbuf);
    free(tbuf);
    free(rbuf);
    free(rtbuf);

    printf("{"
           "\"streams_discovered\":%d,"
           "\"primary_type\":\"%s\","
           "\"channels\":%d,"
           "\"format\":\"%s\","
           "\"nominal_srate\":%.2f,"
           "\"effective_srate\":%.2f,"
           "\"duration_sec\":%.2f,"
           "\"samples_received\":%llu,"
           "\"chunks_received\":%llu,"
           "\"samples_lost\":%lld,"
           "\"timestamp_gaps\":%llu,"
           "\"finite_pct\":%.2f,"
           "\"signal_rms\":%.4f,"
           "\"signal_min\":%.4f,"
           "\"signal_max\":%.4f,"
           "\"chunk_dt_ms\":{\"p50\":%.3f,\"p95\":%.3f,\"p99\":%.3f,\"max\":%.3f},"
           "\"step_err_ms\":{\"p50\":%.4f,\"p99\":%.4f,\"max\":%.4f},"
           "\"comp_active\":%d,"
           "\"comp_ratio\":%.4f,"
           "\"inlet_cpu_pct\":%.2f,"
           "\"sync_streams\":%d,"
           "\"sync_real_pair\":%d,"
           "\"sync_rows\":%llu,"
           "\"sync_hw_gaps\":%llu,"
           "\"sync_cpu_pct\":%.2f,"
           "\"rss_mb\":%.1f"
           "}\n",
           n_found, ptype, ch, fmt_name(fmt), srate, eff_hz, wall_dt,
           (unsigned long long)total_samples, (unsigned long long)total_chunks,
           (long long)samples_lost, (unsigned long long)ts_gaps,
           total_vals > 0 ? (100.0 * (double)finite_vals / (double)total_vals) : 0.0,
           rms_v, finite_vals > 0 ? min_v : 0.0, finite_vals > 0 ? max_v : 0.0,
           pct(chunk_dt_ms, n_chunk_dt, 0.50), pct(chunk_dt_ms, n_chunk_dt, 0.95),
           pct(chunk_dt_ms, n_chunk_dt, 0.99), pct(chunk_dt_ms, n_chunk_dt, 1.00),
           pct(step_err_ms, n_step_err, 0.50), pct(step_err_ms, n_step_err, 0.99),
           pct(step_err_ms, n_step_err, 1.00),
           comp_active, comp_ratio, p1_cpu,
           sync_streams, sync_real_pair,
           (unsigned long long)sync_rows, (unsigned long long)sync_hw_gaps,
           sync_cpu, sync_rss);
    fflush(stdout);
    free(chunk_dt_ms);
    free(step_err_ms);
    return 0;
}
