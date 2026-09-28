/* Drive every description tree call of the C ABI and print what comes back.
 *
 * This program uses nothing but `lsl_c.h`. Link it against real liblsl and
 * against the Rust library, then compare the two outputs. A difference is a
 * conformance failure.
 *
 * The tree calls are easy to get wrong in ways that still look right:
 * `lsl_append_child_value` returns the parent and not the new child,
 * `lsl_value` returns nothing for a tag, and `lsl_set_value` fails on a tag.
 * A program that only builds a tree and writes it out cannot see any of them.
 *
 * Build:
 *   cc -I .build/install/include oracle/desctree.c -o .build/desctree \
 *      -L DIR -llsl -Wl,-rpath,DIR
 */

#include <stdio.h>
#include <string.h>

#include <lsl_c.h>

static const char *nz(const char *s) { return s ? s : "(null)"; }

/* Show a node in one line, without printing a pointer value. Two libraries
 * never place a node at the same address. */
static void show(const char *what, lsl_xml_ptr e) {
	printf("%-22s empty=%d text=%d name=%-14s value=%-14s child_value=%s\n", what, lsl_empty(e),
		lsl_is_text(e), nz(lsl_name(e)), nz(lsl_value(e)), nz(lsl_child_value(e)));
}

int main(void) {
	lsl_streaminfo info =
		lsl_create_streaminfo("Tree", "Interop", 3, 100.0, cft_float32, "tree_src");
	lsl_xml_ptr desc = lsl_get_desc(info);

	printf("=== an empty tree\n");
	show("desc", desc);
	printf("first_child empty  %d\n", lsl_empty(lsl_first_child(desc)));
	printf("last_child empty   %d\n", lsl_empty(lsl_last_child(desc)));
	printf("parent empty       %d\n", lsl_empty(lsl_parent(desc)));
	printf("parent name        %s\n", nz(lsl_name(lsl_parent(desc))));
	printf("parent is text     %d\n", lsl_is_text(lsl_parent(desc)));
	printf("grandparent empty  %d\n", lsl_empty(lsl_parent(lsl_parent(desc))));
	printf("grandparent name   %s\n", nz(lsl_name(lsl_parent(lsl_parent(desc)))));
	printf("above that empty   %d\n", lsl_empty(lsl_parent(lsl_parent(lsl_parent(desc)))));
	printf("desc from parent   %d\n", lsl_child(lsl_parent(desc), "desc") == desc);
	printf("parent name field  %s\n", nz(lsl_child_value_n(lsl_parent(desc), "name")));
	printf("child('x') empty   %d\n", lsl_empty(lsl_child(desc, "x")));
	printf("null name          %s\n", nz(lsl_name(NULL)));
	printf("null empty         %d\n", lsl_empty(NULL));

	printf("\n=== what append_child_value returns\n");
	lsl_xml_ptr ret = lsl_append_child_value(desc, "manufacturer", "Acme");
	printf("returns the parent %d\n", ret == desc);
	printf("returns the child  %d\n", ret == lsl_child(desc, "manufacturer"));

	printf("\n=== a channel list\n");
	lsl_xml_ptr chns = lsl_append_child(desc, "channels");
	const char *labels[] = {"C3", "C4", "Cz"};
	for (int i = 0; i < 3; i++) {
		lsl_xml_ptr c = lsl_append_child(chns, "channel");
		lsl_append_child_value(c, "label", labels[i]);
		lsl_append_child_value(c, "unit", "microvolts");
		lsl_append_child_value(c, "index", "0");
	}
	show("channels", chns);
	show("first channel", lsl_first_child(chns));
	show("last channel", lsl_last_child(chns));

	printf("\n=== walking the siblings forward\n");
	for (lsl_xml_ptr c = lsl_first_child(chns); !lsl_empty(c); c = lsl_next_sibling(c))
		printf("  label %s unit %s\n", nz(lsl_child_value_n(c, "label")),
			nz(lsl_child_value_n(c, "unit")));

	printf("\n=== walking the siblings backward\n");
	for (lsl_xml_ptr c = lsl_last_child(chns); !lsl_empty(c); c = lsl_previous_sibling(c))
		printf("  label %s\n", nz(lsl_child_value_n(c, "label")));

	printf("\n=== a named step\n");
	lsl_xml_ptr first = lsl_first_child(chns);
	printf("next 'channel'     %s\n", nz(lsl_child_value_n(lsl_next_sibling_n(first, "channel"), "label")));
	printf("next 'nothing'     %d\n", lsl_empty(lsl_next_sibling_n(first, "nothing")));
	lsl_xml_ptr last = lsl_last_child(chns);
	printf("previous 'channel' %s\n",
		nz(lsl_child_value_n(lsl_previous_sibling_n(last, "channel"), "label")));

	printf("\n=== the way up\n");
	lsl_xml_ptr label = lsl_child(first, "label");
	show("label", label);
	show("label parent", lsl_parent(label));
	show("label text node", lsl_first_child(label));
	printf("parent of channels is desc %d\n", lsl_parent(chns) == desc);

	printf("\n=== what can be changed\n");
	printf("set_value on a tag   %d\n", lsl_set_value(label, "nope"));
	printf("set_value on text    %d\n", lsl_set_value(lsl_first_child(label), "C9"));
	printf("label now            %s\n", nz(lsl_child_value(label)));
	printf("set_name on a tag    %d\n", lsl_set_name(label, "tag"));
	printf("name now             %s\n", nz(lsl_name(label)));
	printf("set_name back        %d\n", lsl_set_name(label, "label"));
	printf("set_child_value ok   %d\n", lsl_set_child_value(first, "unit", "volts"));
	printf("unit now             %s\n", nz(lsl_child_value_n(first, "unit")));
	printf("set_child_value miss %d\n", lsl_set_child_value(first, "absent", "x"));
	printf("set_child_value tag  %d\n", lsl_set_child_value(desc, "channels", "x"));

	printf("\n=== prepend and copy\n");
	lsl_xml_ptr head = lsl_prepend_child(desc, "head");
	lsl_append_child_value(head, "k", "v");
	printf("first child of desc  %s\n", nz(lsl_name(lsl_first_child(desc))));
	lsl_prepend_child_value(desc, "top", "t");
	printf("first child of desc  %s\n", nz(lsl_name(lsl_first_child(desc))));
	lsl_xml_ptr copy = lsl_append_copy(desc, first);
	printf("copy label           %s\n", nz(lsl_child_value_n(copy, "label")));
	printf("copy is last         %d\n", copy == lsl_last_child(desc));
	lsl_xml_ptr copy2 = lsl_prepend_copy(desc, head);
	printf("copy2 k              %s\n", nz(lsl_child_value_n(copy2, "k")));
	printf("copy2 is first       %d\n", copy2 == lsl_first_child(desc));

	printf("\n=== removing\n");
	lsl_remove_child_n(desc, "top");
	printf("top gone             %d\n", lsl_empty(lsl_child(desc, "top")));
	lsl_remove_child(desc, lsl_child(desc, "head"));
	printf("head gone            %d\n", lsl_empty(lsl_child(desc, "head")));
	lsl_remove_child_n(desc, "absent");
	printf("still has channels   %d\n", !lsl_empty(lsl_child(desc, "channels")));

	printf("\n=== the document\n");
	char *xml = lsl_get_xml(info);
	/* The fields that change every run are cut out, so the two runs compare. */
	const char *fields[] = {"created_at", "uid", "hostname"};
	char buf[65536];
	strncpy(buf, xml ? xml : "", sizeof(buf) - 1);
	buf[sizeof(buf) - 1] = 0;
	for (int i = 0; i < 3; i++) {
		char open[64], close[64];
		snprintf(open, sizeof(open), "<%s>", fields[i]);
		snprintf(close, sizeof(close), "</%s>", fields[i]);
		char *a = strstr(buf, open);
		if (!a) continue;
		char *b = strstr(a, close);
		if (!b) continue;
		char tail[65536];
		strncpy(tail, b, sizeof(tail) - 1);
		tail[sizeof(tail) - 1] = 0;
		snprintf(a + strlen(open), sizeof(buf) - (a + strlen(open) - buf), "PINNED%s", tail);
	}
	printf("%s", buf);
	lsl_destroy_string(xml);

	lsl_destroy_streaminfo(info);
	return 0;
}
