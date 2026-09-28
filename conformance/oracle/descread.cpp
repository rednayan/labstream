// Probe what the description reader keeps.
//
// `stream_info::from_xml` loads the document with pugixml and its default
// parse flags (src/stream_info_impl.cpp:186). `as_xml` writes the same
// document back out. A document that goes in and comes out shows what the
// reader keeps and what it drops.
//
// Build:
//   c++ -std=c++17 -I .build/install/include oracle/descread.cpp \
//       -o .build/descread -L .build/install/lib -llsl \
//       -Wl,-rpath,$PWD/.build/install/lib

#include <fstream>
#include <iostream>
#include <lsl_cpp.h>
#include <string>

static const char *HEAD =
	"<?xml version=\"1.0\"?>\n<info>\n"
	"\t<name>Probe</name>\n"
	"\t<type>Interop</type>\n"
	"\t<channel_count>1</channel_count>\n"
	"\t<channel_format>int8</channel_format>\n"
	"\t<source_id>probe_src</source_id>\n"
	"\t<nominal_srate>0.000000000000000</nominal_srate>\n"
	"\t<version>1.100000000000000</version>\n"
	"\t<created_at>1.000000000000000</created_at>\n"
	"\t<uid>abc123</uid>\n"
	"\t<session_id>default</session_id>\n"
	"\t<hostname>box</hostname>\n"
	"\t<v4address></v4address>\n"
	"\t<v4data_port>16572</v4data_port>\n"
	"\t<v4service_port>16572</v4service_port>\n"
	"\t<v6address></v6address>\n"
	"\t<v6data_port>0</v6data_port>\n"
	"\t<v6service_port>0</v6service_port>\n";

int main(int argc, char **argv) {
	std::string dir = argc > 1 ? argv[1] : ".";
	std::ofstream out(dir + "/desc_reader.txt", std::ios::binary);

	struct Case {
		const char *name;
		const char *desc;
	};
	const Case cases[] = {
		{"unterminated_reference", "<desc><v>100% &amp fine</v></desc>"},
		{"mixed_content", "<desc><v>a<b />c</v></desc>"},
		{"whitespace_only_text", "<desc><v>   </v></desc>"},
		{"attribute", "<desc><v id=\"1\" unit='mV'>x</v></desc>"},
		{"cdata", "<desc><v><![CDATA[a & <b>]]></v></desc>"},
		{"comment", "<desc><!-- gone --><v>x</v></desc>"},
		{"numeric_reference", "<desc><v>&#65;&#x42;&#10;</v></desc>"},
		{"nested_empty", "<desc><a></a><b/></desc>"},
		{"attribute_escapes",
			"<desc><v a=\"x&amp;y&quot;z&#10;w&#9;t&lt;u&gt;\" b='s&apos;q'>1</v></desc>"},
		{"attribute_only", "<desc><v a=\"1\" /></desc>"},
		{"cdata_and_text", "<desc><v>a<![CDATA[<b>]]>c</v></desc>"},
	};

	for (const Case &c : cases) {
		std::string xml = std::string(HEAD) + "\t" + c.desc + "\n</info>\n";
		lsl::stream_info back = lsl::stream_info::from_xml(xml);
		std::string got = back.as_xml();
		out << "===== " << c.name << "\n";
		out << "--- name: " << back.name() << "\n";
		// Only the <desc> part matters here.
		auto a = got.find("<desc");
		auto b = got.find("</info>");
		out << (a == std::string::npos ? std::string("(no desc)") : got.substr(a, b - a));
		out << "\n";
	}

	std::cout << "wrote the reader probe to " << dir << std::endl;
	return 0;
}
