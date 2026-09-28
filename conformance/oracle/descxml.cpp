// Capture the full description document that liblsl writes.
//
// The short description that a discovery answer carries always holds an empty
// `<desc />`. The whole document travels on the data port, in answer to
// `LSL:fullinfo` (src/tcp_server.cpp:508). `stream_info::as_xml` returns that
// same document (src/lsl_streaminfo_c.cpp:63).
//
// This program builds one tree for each case below and writes the document to
// a file. The Rust writer must produce the same bytes.
//
// Build:
//   c++ -std=c++17 -I .build/install/include oracle/descxml.cpp \
//       -o .build/descxml -L .build/install/lib -llsl \
//       -Wl,-rpath,$PWD/.build/install/lib

#include <fstream>
#include <iostream>
#include <lsl_cpp.h>
#include <string>

// The fields that carry a clock reading or a random identifier change on every
// run. Replace them, so the capture is reproducible.
static std::string pin(std::string xml) {
	const char *fields[] = {"created_at", "uid", "hostname", "v4address", "v4data_port",
		"v4service_port", "v6address", "v6data_port", "v6service_port"};
	for (const char *f : fields) {
		std::string open = std::string("<") + f + ">";
		std::string close = std::string("</") + f + ">";
		auto a = xml.find(open);
		if (a == std::string::npos) continue;
		auto b = xml.find(close, a);
		if (b == std::string::npos) continue;
		xml.replace(a + open.size(), b - a - open.size(), "PINNED");
	}
	return xml;
}

static void save(const std::string &dir, const std::string &name, lsl::stream_info &info) {
	std::ofstream out(dir + "/" + name + ".xml", std::ios::binary);
	out << pin(info.as_xml());
}

int main(int argc, char **argv) {
	std::string dir = argc > 1 ? argv[1] : ".";

	{
		// No tree at all. This is what an outlet publishes when the application
		// adds nothing.
		lsl::stream_info info("Desc", "Interop", 2, 100.0, lsl::cf_float32, "desc_src");
		save(dir, "desc_empty", info);
	}
	{
		// One level of text children.
		lsl::stream_info info("Desc", "Interop", 2, 100.0, lsl::cf_float32, "desc_src");
		info.desc().append_child_value("manufacturer", "Acme");
		info.desc().append_child_value("serial", "0042");
		save(dir, "desc_flat", info);
	}
	{
		// The shape that a real device writes: a channel list.
		lsl::stream_info info("Desc", "Interop", 3, 100.0, lsl::cf_float32, "desc_src");
		lsl::xml_element chns = info.desc().append_child("channels");
		const char *labels[] = {"C3", "C4", "Cz"};
		for (const char *l : labels) {
			lsl::xml_element c = chns.append_child("channel");
			c.append_child_value("label", l);
			c.append_child_value("unit", "microvolts");
			c.append_child_value("type", "EEG");
		}
		info.desc().append_child_value("manufacturer", "Acme");
		save(dir, "desc_channels", info);
	}
	{
		// An empty text value, and a branch that holds nothing.
		lsl::stream_info info("Desc", "Interop", 1, 0.0, lsl::cf_string, "desc_src");
		info.desc().append_child_value("blank", "");
		info.desc().append_child("hollow");
		info.desc().append_child("outer").append_child("inner");
		save(dir, "desc_blank", info);
	}
	{
		// The five characters that XML reserves, plus control characters.
		lsl::stream_info info("Desc", "Interop", 1, 0.0, lsl::cf_int8, "desc_src");
		info.desc().append_child_value("amp", "a & b");
		info.desc().append_child_value("angles", "<tag> & </tag>");
		info.desc().append_child_value("quotes", "he said \"hi\" and 'bye'");
		info.desc().append_child_value("lines", "one\ntwo\rthree\tfour");
		info.desc().append_child_value("utf8", "\xc2\xb5V \xe2\x88\x86");
		save(dir, "desc_escapes", info);
	}
	{
		// Deep nesting, to fix the indent rule at every level.
		lsl::stream_info info("Desc", "Interop", 1, 0.0, lsl::cf_int8, "desc_src");
		lsl::xml_element n = info.desc();
		for (int i = 0; i < 6; i++) n = n.append_child("level" + std::to_string(i));
		n.append_child_value("leaf", "bottom");
		save(dir, "desc_deep", info);
	}
	{
		// Two children with the same name at the top, and a name that carries
		// characters which XML allows but which are unusual.
		lsl::stream_info info("Desc", "Interop", 1, 0.0, lsl::cf_int8, "desc_src");
		info.desc().append_child_value("item", "one");
		info.desc().append_child_value("item", "two");
		info.desc().append_child_value("dotted.name_2-x", "value");
		save(dir, "desc_repeat", info);
	}
	{
		// A description that comes back from XML.
		//
		// A stream_info that no outlet published carries an empty <uid>, and
		// read_xml rejects that (src/stream_info_impl.cpp:137). Fill the field
		// first, so the reader accepts the document.
		lsl::stream_info info("Desc", "Interop", 2, 100.0, lsl::cf_float32, "desc_src");
		info.desc().append_child("channels").append_child("channel").append_child_value(
			"label", "C3");
		std::string xml = info.as_xml();
		auto a = xml.find("<uid>");
		xml.replace(a, 11, "<uid>abc123</uid>");
		lsl::stream_info back = lsl::stream_info::from_xml(xml);
		save(dir, "desc_roundtrip", back);
		std::ofstream nm(dir + "/desc_roundtrip.name", std::ios::binary);
		nm << back.name() << "\n";
	}
	{
		// The same document with the empty <uid> left in place. The reader
		// throws, resets every field, and writes the reason into the name.
		lsl::stream_info info("Desc", "Interop", 2, 100.0, lsl::cf_float32, "desc_src");
		lsl::stream_info back = lsl::stream_info::from_xml(info.as_xml());
		save(dir, "desc_nouid", back);
		std::ofstream nm(dir + "/desc_nouid.name", std::ios::binary);
		nm << back.name() << "\n";
	}

	{
		// What survives a round trip through the reader. pugixml normalizes
		// line ends and decodes entity references, so the text that comes back
		// can differ from the text that went in.
		lsl::stream_info info("Desc", "Interop", 1, 0.0, lsl::cf_int8, "desc_src");
		info.desc().append_child_value("amp", "a & b");
		info.desc().append_child_value("angles", "<tag> & </tag>");
		info.desc().append_child_value("quotes", "he said \"hi\" and 'bye'");
		info.desc().append_child_value("lines", "one\ntwo\rthree\tfour");
		info.desc().append_child_value("utf8", "\xc2\xb5V \xe2\x88\x86");
		std::string xml = info.as_xml();
		auto a = xml.find("<uid>");
		xml.replace(a, 11, "<uid>abc123</uid>");
		lsl::stream_info back = lsl::stream_info::from_xml(xml);
		save(dir, "desc_reparse", back);

		std::ofstream hex(dir + "/desc_reparse.hex", std::ios::binary);
		const char *keys[] = {"amp", "angles", "quotes", "lines", "utf8"};
		for (const char *k : keys) {
			std::string v = back.desc().child_value(k);
			hex << k << " ";
			for (unsigned char c : v) {
				const char *digits = "0123456789abcdef";
				hex << digits[c >> 4] << digits[c & 15];
			}
			hex << "\n";
		}
	}

	std::cout << "wrote the captures to " << dir << std::endl;
	return 0;
}
