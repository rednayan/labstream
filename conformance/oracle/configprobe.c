/* Report what a configuration file changed.
 *
 * liblsl reads one file at the first call that needs a value from it
 * (`src/api_config.cpp:326`). The file decides the ports, the session, and
 * several tuning values. A site that edits `lsl_api.cfg` edits the wire, so
 * two libraries that read the file differently cannot see each other.
 *
 * This program publishes one stream and prints what the configuration did to
 * it. Link it against each library, point `LSLAPICFG` at the same file, and
 * compare the two outputs.
 *
 * Build:
 *   cc -I .build/install/include oracle/configprobe.c -o .build/configprobe \
 *      -L DIR -llsl -Wl,-rpath,DIR
 */

#include <stdio.h>
#include <string.h>

#include <lsl_c.h>

int main(int argc, char *argv[]) {
	const char *name = argc > 1 ? argv[1] : "ConfigProbe";

	lsl_streaminfo info =
		lsl_create_streaminfo(name, "Probe", 2, 100.0, cft_float32, "configprobe_src");
	lsl_outlet out = lsl_create_outlet(info, 0, 360);
	if (!out) {
		printf("outlet: none\n");
		return 1;
	}

	lsl_streaminfo published = lsl_get_info(out);
	int base = lsl_get_channel_count(published); /* keeps the handle in use */
	(void)base;

	/* The ports come from `ports.BasePort` and the range that follows it. The
	 * exact number depends on what else is bound, so only the block is
	 * printed. */
	char *xml = lsl_get_xml(published);
	int data_port = 0, service_port = 0;
	const char *p = strstr(xml, "<v4data_port>");
	if (p) sscanf(p + 13, "%d", &data_port);
	p = strstr(xml, "<v4service_port>");
	if (p) sscanf(p + 16, "%d", &service_port);
	printf("data port block   %d\n", data_port / 100 * 100);
	printf("service port block %d\n", service_port / 100 * 100);
	printf("session id        %s\n", lsl_get_session_id(published));

	/* A resolver only sees a stream of its own session, so this also tests
	 * that both sides read `lab.SessionID` the same way. */
	lsl_streaminfo found[4];
	int n = lsl_resolve_byprop(found, 4, "name", name, 1, 5.0);
	printf("resolved          %d\n", n > 0 ? 1 : 0);

	if (n > 0) {
		lsl_inlet in = lsl_create_inlet(found[0], 360, 0, 1);
		int ec = 0;
		lsl_open_stream(in, 5.0, &ec);
		/* The outlet drops a sample that it sends before the consumer is
		 * registered, so wait for the connection first. */
		lsl_wait_for_consumers(out, 5.0);
		float sample[2] = {1.0f, 2.0f};
		lsl_push_sample_ft(out, sample, 7.125);
		float got[2] = {0, 0};
		double ts = lsl_pull_sample_f(in, got, 2, 5.0, &ec);
		/* `tuning.ForceDefaultTimestamps` replaces every pushed value with the
		 * current clock (`src/stream_outlet_impl.cpp:162`). */
		printf("timestamp kept    %d\n", ts == 7.125 ? 1 : 0);
		printf("values kept       %d\n", (got[0] == 1.0f && got[1] == 2.0f) ? 1 : 0);
		lsl_destroy_inlet(in);
		for (int k = 0; k < n; k++) lsl_destroy_streaminfo(found[k]);
	} else {
		printf("timestamp kept    -\n");
		printf("values kept       -\n");
	}

	lsl_destroy_string(xml);
	lsl_destroy_streaminfo(published);
	lsl_destroy_outlet(out);
	lsl_destroy_streaminfo(info);
	return 0;
}
