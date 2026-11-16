/* Measure the time from a push in one process to a pull in another.
 *
 * The program runs as an outlet or as an inlet. Link it once against real
 * liblsl and once against the Rust library. The same program then measures
 * both libraries, and the two can also measure each other.
 *
 * The clock is CLOCK_MONOTONIC of this program, not the clock of the
 * library. The outlet passes the push time as the timestamp of each sample.
 * The inlet does no post-processing, so it reads the push time back
 * unchanged. Both processes run on one machine and read the same clock.
 *
 * Channel 0 carries the sample number. The inlet uses it to count lost,
 * repeated, and reordered samples.
 *
 * Each process writes one JSON line to stdout when it ends.
 * `oracle/latency.py` runs the pairs and makes the table.
 *
 * Build:
 *   cc -O2 -I .build/install/include oracle/latency.c -o BIN -lm -ldl \
 *      -L DIR -llsl -Wl,-rpath,DIR
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>

#include <lsl_c.h>

static double now(void) {
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (double)t.tv_sec + (double)t.tv_nsec * 1e-9;
}

static void sleep_until(double t) {
	struct timespec ts;
	ts.tv_sec = (time_t)t;
	ts.tv_nsec = (long)((t - (double)ts.tv_sec) * 1e9);
	while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &ts, NULL) != 0) {
	}
}

/* User plus system CPU time of this process, in seconds. */
static double cpu_seconds(void) {
	struct rusage r;
	getrusage(RUSAGE_SELF, &r);
	return (double)r.ru_utime.tv_sec + (double)r.ru_utime.tv_usec * 1e-6 +
		   (double)r.ru_stime.tv_sec + (double)r.ru_stime.tv_usec * 1e-6;
}

/* The file of the library that the loader picked. An earlier run used the
 * wrong library through a stale RUNPATH, so each row names its library. */
static const char *library_path(void) {
	Dl_info d;
	if (dladdr((void *)lsl_library_info, &d) && d.dli_fname) return d.dli_fname;
	return "unknown";
}

static int cmp_double(const void *a, const void *b) {
	double x = *(const double *)a, y = *(const double *)b;
	return (x > y) - (x < y);
}

/* The value at fraction q of a sorted array. */
static double quantile(const double *v, long n, double q) {
	if (n <= 0) return NAN;
	long k = (long)(q * (double)(n - 1) + 0.5);
	return v[k];
}

static void print_library(void) {
	printf("\"library\": \"%s\", \"library_info\": \"%s\"", library_path(), lsl_library_info());
}

static int run_outlet(const char *name, int channels, double rate, int chunk, double seconds,
	double offset_us) {
	lsl_streaminfo info =
		lsl_create_streaminfo(name, "Latency", channels, rate, cft_float32, name);
	lsl_outlet out = lsl_create_outlet(info, 0, 360);
	if (!out) {
		fprintf(stderr, "ERROR could not create the outlet\n");
		return 1;
	}
	fprintf(stderr, "READY\n");
	if (!lsl_wait_for_consumers(out, 20.0)) {
		fprintf(stderr, "ERROR no consumer in 20 s\n");
		return 1;
	}

	long total = (long)(rate * seconds);
	long pushes = (total + chunk - 1) / chunk;
	float *data = calloc((size_t)chunk * (size_t)channels, sizeof(float));
	double *stamps = calloc((size_t)chunk, sizeof(double));
	double *lag = calloc((size_t)pushes, sizeof(double));

	/* The inlet starts its reader at the first pull, so the first samples
	 * wait in the outlet for a short time. Give it time to start. */
	sleep_until(now() + 0.5);

	double cpu0 = cpu_seconds();
	double t0 = now();
	long k = 0;
	for (long p = 0; p < pushes; p++) {
		long n = total - k < chunk ? total - k : chunk;
		double due = t0 + (double)(k + n) / rate;
		sleep_until(due);
		for (long i = 0; i < n; i++) {
			data[i * channels] = (float)(k + i);
			for (int c = 1; c < channels; c++) data[i * channels + c] = (float)c;
		}
		double t = now();
		lag[p] = t - due;
		for (long i = 0; i < n; i++) stamps[i] = t - offset_us * 1e-6;
		if (n == 1)
			lsl_push_sample_ft(out, data, stamps[0]);
		else
			lsl_push_chunk_ftn(out, data, (unsigned long)(n * channels), stamps);
		k += n;
	}
	double wall = now() - t0;
	double cpu = cpu_seconds() - cpu0;

	/* Let the last samples leave before the outlet closes. */
	sleep_until(now() + 1.0);

	qsort(lag, (size_t)pushes, sizeof(double), cmp_double);
	printf("{\"role\": \"outlet\", ");
	print_library();
	printf(", \"sent\": %ld, \"pushes\": %ld, \"wall_s\": %.3f, \"cpu_pct\": %.3f, "
		   "\"lag_p50_us\": %.1f, \"lag_p99_us\": %.1f, \"lag_max_us\": %.1f}\n",
		total, pushes, wall, 100.0 * cpu / wall, quantile(lag, pushes, 0.5) * 1e6,
		quantile(lag, pushes, 0.99) * 1e6, lag[pushes - 1] * 1e6);
	fflush(stdout);
	lsl_destroy_outlet(out);
	return 0;
}

static int run_inlet(const char *name, int channels, double rate, double seconds,
	double warmup) {
	lsl_streaminfo found[1];
	if (lsl_resolve_byprop(found, 1, "name", name, 1, 20.0) < 1) {
		fprintf(stderr, "ERROR the stream %s was not found\n", name);
		return 1;
	}
	lsl_inlet in = lsl_create_inlet(found[0], 360, 0, 1);
	int32_t ec = 0;
	lsl_open_stream(in, 20.0, &ec);
	if (ec) {
		fprintf(stderr, "ERROR open_stream gave %d\n", ec);
		return 1;
	}
	fprintf(stderr, "READY\n");

	long total = (long)(rate * seconds);
	long skip = (long)(rate * warmup);
	double *lat = calloc((size_t)total + 1, sizeof(double));
	unsigned char *seen = calloc((size_t)total + 1, 1);
	float *buf = calloc((size_t)channels, sizeof(float));
	long got = 0, kept = 0, repeated = 0, reordered = 0, alien = 0;
	long last = -1;
	double cpu0 = 0, t0 = 0;

	for (;;) {
		double ts = lsl_pull_sample_f(in, buf, channels, 3.0, &ec);
		double t = now();
		if (ts == 0.0 || ec) break; /* no sample in 3 s: the outlet is gone */
		if (got == 0) {
			cpu0 = cpu_seconds();
			t0 = t;
		}
		got++;
		long idx = (long)buf[0];
		if (idx < 0 || idx >= total) {
			alien++;
			continue;
		}
		if (seen[idx]) {
			repeated++;
			continue;
		}
		seen[idx] = 1;
		if (idx < last) reordered++;
		last = idx;
		if (idx >= skip) lat[kept++] = t - ts;
		if (idx == total - 1) break;
	}
	double wall = now() - t0;
	double cpu = cpu_seconds() - cpu0;

	long lost = 0;
	for (long i = 0; i < total; i++)
		if (!seen[i]) lost++;

	qsort(lat, (size_t)kept, sizeof(double), cmp_double);
	printf("{\"role\": \"inlet\", ");
	print_library();
	printf(", \"expected\": %ld, \"received\": %ld, \"lost\": %ld, \"repeated\": %ld, "
		   "\"reordered\": %ld, \"alien\": %ld, \"measured\": %ld, \"wall_s\": %.3f, "
		   "\"cpu_pct\": %.3f, \"p50_us\": %.1f, \"p90_us\": %.1f, \"p99_us\": %.1f, "
		   "\"p999_us\": %.1f, \"max_us\": %.1f, \"min_us\": %.1f}\n",
		total, got, lost, repeated, reordered, alien, kept, wall,
		wall > 0 ? 100.0 * cpu / wall : 0.0, quantile(lat, kept, 0.5) * 1e6,
		quantile(lat, kept, 0.9) * 1e6, quantile(lat, kept, 0.99) * 1e6,
		quantile(lat, kept, 0.999) * 1e6, kept ? lat[kept - 1] * 1e6 : NAN,
		kept ? lat[0] * 1e6 : NAN);
	fflush(stdout);
	lsl_destroy_inlet(in);
	return 0;
}

int main(int argc, char **argv) {
	const char *role = NULL;
	const char *name = "LatencyBench";
	int channels = 8;
	double rate = 512.0;
	int chunk = 1;
	double seconds = 10.0;
	double warmup = 1.0;
	double offset_us = 0.0;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "outlet") || !strcmp(argv[i], "inlet")) {
			role = argv[i];
			continue;
		}
		if (i + 1 >= argc) {
			fprintf(stderr, "the option %s has no value\n", argv[i]);
			return 2;
		}
		if (!strcmp(argv[i], "--name")) name = argv[++i];
		else if (!strcmp(argv[i], "--channels")) channels = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--rate")) rate = atof(argv[++i]);
		else if (!strcmp(argv[i], "--chunk")) chunk = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--seconds")) seconds = atof(argv[++i]);
		else if (!strcmp(argv[i], "--warmup")) warmup = atof(argv[++i]);
		else if (!strcmp(argv[i], "--offset-us")) offset_us = atof(argv[++i]);
		else {
			fprintf(stderr, "unknown option %s\n", argv[i]);
			return 2;
		}
	}
	if (!role || channels < 1 || rate <= 0 || chunk < 1 || seconds <= 0) {
		fprintf(stderr, "usage: latency outlet|inlet [--name N] [--channels C] [--rate R] "
						"[--chunk K] [--seconds S] [--warmup S] [--offset-us U]\n");
		return 2;
	}
	if (!strcmp(role, "outlet")) return run_outlet(name, channels, rate, chunk, seconds, offset_us);
	return run_inlet(name, channels, rate, seconds, warmup);
}
