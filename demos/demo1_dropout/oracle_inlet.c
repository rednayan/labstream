/* Genuine C++ liblsl inlet sidecar for Demo 1 (Machine B).
 *
 * Links against the pinned liblsl shared library in conformance/.build/install/lib/
 * and emits compact JSON-lines to stdout so demo1-viewer can plot real liblsl
 * side by side against labstream over the exact same physical link.
 */
#include <lsl_c.h>
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static pthread_mutex_t out_lock = PTHREAD_MUTEX_INITIALIZER;

static void *marker_thread(void *arg) {
    lsl_streaminfo info = (lsl_streaminfo)arg;
    lsl_inlet inlet = lsl_create_inlet(info, 360, 0, 1);
    int32_t ec = 0;
    lsl_open_stream(inlet, 5.0, &ec);

    char *sample_str = NULL;
    for (;;) {
        double ts = lsl_pull_sample_str(inlet, &sample_str, 1, 0.5, &ec);
        if (ts != 0.0 && sample_str != NULL) {
            pthread_mutex_lock(&out_lock);
            printf("{\"k\":\"m\",\"ts\":%.6f,\"text\":\"%s\"}\n", ts, sample_str);
            fflush(stdout);
            pthread_mutex_unlock(&out_lock);
            lsl_destroy_string(sample_str);
            sample_str = NULL;
        }
    }
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    lsl_streaminfo eeg_info = NULL;
    lsl_streaminfo marker_info = NULL;
    lsl_streaminfo found[32];

    fprintf(stderr, "[oracle_inlet] resolving Demo1EEG and Demo1Markers via liblsl...\n");
    while (eeg_info == NULL || marker_info == NULL) {
        int n = lsl_resolve_all(found, 32, 1.0);
        for (int i = 0; i < n; i++) {
            const char *name = lsl_get_name(found[i]);
            if (eeg_info == NULL && name && strcmp(name, "Demo1EEG") == 0) {
                eeg_info = found[i];
            } else if (marker_info == NULL && name && strcmp(name, "Demo1Markers") == 0) {
                marker_info = found[i];
            } else {
                lsl_destroy_streaminfo(found[i]);
            }
        }
    }
    fprintf(stderr, "[oracle_inlet] connected to Demo1EEG and Demo1Markers via liblsl\n");

    pthread_t mth;
    pthread_create(&mth, NULL, marker_thread, marker_info);

    lsl_inlet inlet = lsl_create_inlet(eeg_info, 360, 0, 1);
    int32_t ec = 0;
    lsl_open_stream(inlet, 5.0, &ec);

    float data[2 * 64];
    double stamps[64];
    int64_t expected_seq = -1;
    uint64_t total_rx = 0;
    uint64_t total_lost = 0;

    for (;;) {
        /* lsl_pull_chunk_f returns the number of float elements (samples * 2). */
        unsigned long elems = lsl_pull_chunk_f(inlet, data, stamps, 128, 64, 0.1, &ec);
        unsigned long got = elems / 2;
        if (got == 0) {
            pthread_mutex_lock(&out_lock);
            printf("{\"k\":\"hb\",\"rx\":%llu,\"lost\":%llu}\n",
                   (unsigned long long)total_rx, (unsigned long long)total_lost);
            fflush(stdout);
            pthread_mutex_unlock(&out_lock);
            continue;
        }
        pthread_mutex_lock(&out_lock);
        for (unsigned long i = 0; i < got; i++) {
            float val = data[i * 2];
            int64_t seq = (int64_t)llround((double)data[i * 2 + 1]);
            if (expected_seq >= 0 && seq > expected_seq) {
                int64_t gap = seq - expected_seq;
                total_lost += (uint64_t)gap;
                printf("{\"k\":\"g\",\"from\":%lld,\"to\":%lld,\"lost\":%lld}\n",
                       (long long)expected_seq, (long long)seq, (long long)gap);
            }
            expected_seq = seq + 1;
            total_rx += 1;
            /* Emit every 4th sample (128 Hz) for smooth browser plotting. */
            if ((seq & 3) == 0) {
                printf("{\"k\":\"s\",\"seq\":%lld,\"ts\":%.6f,\"v\":%.5f,\"rx\":%llu,\"lost\":%llu}\n",
                       (long long)seq, stamps[i], (double)val,
                       (unsigned long long)total_rx, (unsigned long long)total_lost);
            }
        }
        fflush(stdout);
        pthread_mutex_unlock(&out_lock);
    }
    return 0;
}
