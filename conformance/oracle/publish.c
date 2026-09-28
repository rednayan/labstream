/* Publish the same signal as `crates/labstream-net/examples/publish.rs`, through the
 * C ABI.
 *
 * The example program `SendData.cpp` of liblsl sends `rand()` noise, so two
 * runs of it never agree and a plot of one cannot be compared with a plot of
 * another. This program sends a signal built from the sample number instead.
 *
 * Link it against real liblsl and against the Rust library. Both then send the
 * same numbers as the native Rust publisher, so all three plots must look the
 * same.
 *
 * Build:
 *   cc -I .build/install/include oracle/publish.c -o BIN -lm \
 *      -L DIR -llsl -Wl,-rpath,DIR
 */

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <lsl_c.h>

/* The eight labels that the Rust publisher writes, in the same order. */
static const char *LABELS[8] = {"Fp1", "Fp2", "C3", "C4", "P3", "P4", "O1", "O2"};

static void label_of(int k, char *out, size_t n) {
	if (k < 8)
		snprintf(out, n, "%s", LABELS[k]);
	else
		snprintf(out, n, "Ch%d", k + 1);
}

int main(int argc, char **argv) {
	const char *name = "RustTest";
	const char *type = "EEG";
	int channels = 8;
	double rate = 100.0;
	double seconds = 0.0; /* zero means no limit */
	int markers = 1;
	/* A word that separates one publisher from another on the same network. */
	const char *tag = "";

	for (int i = 1; i + 1 < argc; i += 2) {
		if (!strcmp(argv[i], "--name")) name = argv[i + 1];
		else if (!strcmp(argv[i], "--type")) type = argv[i + 1];
		else if (!strcmp(argv[i], "--channels")) channels = atoi(argv[i + 1]);
		else if (!strcmp(argv[i], "--rate")) rate = atof(argv[i + 1]);
		else if (!strcmp(argv[i], "--seconds")) seconds = atof(argv[i + 1]);
		else if (!strcmp(argv[i], "--markers")) markers = strcmp(argv[i + 1], "0") != 0;
		else if (!strcmp(argv[i], "--tag")) tag = argv[i + 1];
		else {
			fprintf(stderr, "unknown option %s\n", argv[i]);
			return 2;
		}
	}

	/* The tag separates one publisher from another. With it, the three modes
	 * of `oracle/labrecorder.sh` can run at the same time, and a recorder
	 * shows which library sent which stream. */
	char stream_name[320], source_id[320];
	if (*tag) {
		snprintf(stream_name, sizeof(stream_name), "%s-%s", name, tag);
		snprintf(source_id, sizeof(source_id), "%s_publish_%s", name, tag);
	} else {
		snprintf(stream_name, sizeof(stream_name), "%s", name);
		snprintf(source_id, sizeof(source_id), "%s_publish", name);
	}

	lsl_streaminfo info =
		lsl_create_streaminfo(stream_name, type, channels, rate, cft_float32, source_id);

	lsl_xml_ptr desc = lsl_get_desc(info);
	lsl_xml_ptr chns = lsl_append_child(desc, "channels");
	for (int k = 0; k < channels; k++) {
		char lab[32];
		label_of(k, lab, sizeof(lab));
		lsl_xml_ptr c = lsl_append_child(chns, "channel");
		lsl_append_child_value(c, "label", lab);
		lsl_append_child_value(c, "unit", "microvolts");
		lsl_append_child_value(c, "type", type);
	}
	lsl_append_child_value(desc, "manufacturer", "conformance publisher");
	lsl_xml_ptr acq = lsl_append_child(desc, "acquisition");
	lsl_append_child_value(acq, "model", "publish example");

	lsl_outlet out = lsl_create_outlet(info, 0, 360);
	if (!out) {
		fprintf(stderr, "cannot publish\n");
		return 1;
	}

	/* A marker stream is a second stream: one channel of text, and no rate. A
	 * recorder treats it as events rather than as a signal. */
	char marker_name[352], marker_source[352];
	if (*tag) {
		snprintf(marker_name, sizeof(marker_name), "%s-Markers-%s", name, tag);
		snprintf(marker_source, sizeof(marker_source), "%s_markers_%s", name, tag);
	} else {
		snprintf(marker_name, sizeof(marker_name), "%s-Markers", name);
		snprintf(marker_source, sizeof(marker_source), "%s_markers", name);
	}
	lsl_streaminfo marker_info = NULL;
	lsl_outlet marker_out = NULL;
	if (markers) {
		marker_info = lsl_create_streaminfo(
			marker_name, "Markers", 1, LSL_IRREGULAR_RATE, cft_string, marker_source);
		marker_out = lsl_create_outlet(marker_info, 0, 360);
		if (!marker_out) fprintf(stderr, "cannot publish the markers\n");
	}

	printf("=== the stream is published\n");
	printf("  name          %s\n", stream_name);
	printf("  type          %s\n", type);
	printf("  channels      %d float32\n", channels);
	printf("  rate          %g Hz\n", rate);
	printf("  source id     %s\n", source_id);
	printf("  library       %d\n", lsl_library_version());
	if (marker_out)
		printf("  marker stream %s  (%s)\n                one every second, on a zero "
			   "crossing of channel one\n",
			marker_name, marker_source);
	else
		printf("  marker stream none\n");
	printf("\n=== open LabRecorder on the other machine and press Update\n");
	printf("  The count below rises when a recorder links the stream.\n\n");
	fflush(stdout);

	float *sample = (float *)malloc(sizeof(float) * (size_t)channels);
	double period = rate > 0.0 ? 1.0 / rate : 0.01;
	unsigned long n = 0;
	int consumers = 0;
	/* The wall clock, not `clock()`. `clock()` counts processor time, and a
	 * program that sleeps between samples uses almost none of it. */
	struct timespec t_start;
	clock_gettime(CLOCK_MONOTONIC, &t_start);

	for (;;) {
		struct timespec t_now;
		clock_gettime(CLOCK_MONOTONIC, &t_now);
		double elapsed = (double)(t_now.tv_sec - t_start.tv_sec) +
						 (double)(t_now.tv_nsec - t_start.tv_nsec) * 1e-9;
		if (seconds > 0.0 && elapsed >= seconds) {
			printf("\n%lu samples sent. Stopping.\n", n);
			break;
		}
		int now = lsl_have_consumers(out) ? 1 : 0;
		if (now != consumers) {
			printf("  consumers %d -> %d   after %lu samples\n", consumers, now, n);
			fflush(stdout);
			consumers = now;
		}

		/* The same arithmetic as the Rust publisher, in the same order: the
		 * time comes from the sample number, the sine is computed as a double,
		 * and the result is rounded once to a float. */
		double t = (double)n / (rate > 1.0 ? rate : 1.0);
		for (int k = 0; k < channels; k++) {
			if (k + 1 == channels) {
				sample[k] = (float)n;
			} else {
				double hz = (double)(k + 1);
				sample[k] = (float)(100.0 * sin(2.0 * M_PI * hz * t));
			}
		}
		/* A timestamp of zero means the current clock. */
		lsl_push_sample_f(out, sample);

		/* One marker at each whole second, where the sine of channel one
		 * crosses zero on its way up. The same rule decides its timestamp, so
		 * the two sit within a fraction of one period of each other. */
		unsigned long per_second = (unsigned long)(rate > 1.0 ? rate : 1.0);
		if (marker_out && n % per_second == 0) {
			unsigned long second = n / per_second;
			char text[64];
			snprintf(text, sizeof(text), "%s %lu", second % 5 == 0 ? "burst" : "tick", second);
			const char *one[1] = {text};
			lsl_push_sample_str(marker_out, one);
		}
		n++;

		struct timespec ts;
		ts.tv_sec = (time_t)period;
		ts.tv_nsec = (long)((period - (double)ts.tv_sec) * 1e9);
		nanosleep(&ts, NULL);
	}

	free(sample);
	if (marker_out) lsl_destroy_outlet(marker_out);
	if (marker_info) lsl_destroy_streaminfo(marker_info);
	lsl_destroy_outlet(out);
	lsl_destroy_streaminfo(info);
	return 0;
}
