// The null implementation under test.
//
// This program wraps real liblsl behind the peer interface that the interop
// harness drives. It uses the public C++ API only.
//
// The harness judges this peer first. Both sides are then real liblsl, so
// every cell of the matrix must pass. A failure means the harness holds the
// defect and not the implementation.
//
// A later Rust peer implements the same command line. The harness does not
// change, because it knows nothing about either implementation.
//
// Usage:
//   peer outlet --name N --format F --channels C --srate S --input FILE
//   peer inlet  --name N --format F --channels C --count K --output FILE
//
// The outlet prints READY to standard error, then waits for a line on standard
// input before it pushes. The inlet prints READY once the stream is open.
//
// Every value in a sample file is a bit pattern or an exact integer, so no text
// formatting of a float takes place.

#include <lsl_cpp.h>

#include <chrono>
#include <cstdint>
#include <cstring>
#include <fstream>
#include <iostream>
#include <algorithm>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace {

double bits_to_double(uint64_t b) {
	double d;
	std::memcpy(&d, &b, sizeof(d));
	return d;
}

uint64_t double_to_bits(double d) {
	uint64_t b;
	std::memcpy(&b, &d, sizeof(b));
	return b;
}

std::string hex(uint64_t v) {
	std::ostringstream os;
	os << std::hex << v;
	return os.str();
}

uint64_t unhex(const std::string &s) {
	return std::stoull(s, nullptr, 16);
}

std::string hex_bytes(const std::string &s) {
	static const char *d = "0123456789abcdef";
	std::string out;
	for (unsigned char c : s) {
		out.push_back(d[c >> 4]);
		out.push_back(d[c & 0xF]);
	}
	return out;
}

std::string unhex_bytes(const std::string &s) {
	// A sentinel stands for an empty value, because an empty field would
	// vanish when the reader splits a line on whitespace.
	if (s == "-") return std::string();
	std::string out;
	for (std::size_t i = 0; i + 1 < s.size(); i += 2)
		out.push_back(static_cast<char>(std::stoul(s.substr(i, 2), nullptr, 16)));
	return out;
}

struct Args {
	std::string mode, name, format, input, output, source_id;
	std::string stream_type = "Interop";
	int channels = 1, count = 0;
	double srate = 100.0;
	double timeout = 20.0;
	// Stress options. The interop matrix leaves every one at its default, so
	// a plain run behaves exactly as before.
	int buflen = 3600;       // Max-Buffer-Length. A small value overflows sooner.
	int chunklen = 0;        // Max-Chunk-Length.
	int pull_delay_ms = 0;   // A slow reader fills the queue.
	int push_delay_ms = 4;   // A fast writer overruns a slow reader.
	bool recover = false;    // liblsl defaults this to true.
	bool pushthrough = true; // False lets Max-Chunk-Length decide the write size.
	int consumers = 1;       // Reserved for a caller that runs several inlets.
	int postproc = 0;        // proc_none. The harness turns stages on by number.
	int chunk = 0;           // Push in chunks of this many samples. 0 pushes one at a time.
	bool zero_stamps = false; // Push every sample with a timestamp of zero.
	bool sync = false;       // Open the outlet in the blocking, zero-copy mode.
	bool desc = false;       // Publish a channel tree, and read one back.
	std::string desc_output; // Where the inlet writes the tree it received.
};

Args parse(int argc, char **argv) {
	Args a;
	if (argc < 2) throw std::runtime_error("no mode");
	a.mode = argv[1];
	for (int i = 2; i + 1 < argc; i += 2) {
		std::string k = argv[i], v = argv[i + 1];
		if (k == "--name") a.name = v;
		else if (k == "--format") a.format = v;
		else if (k == "--channels") a.channels = std::stoi(v);
		else if (k == "--srate") a.srate = std::stod(v);
		else if (k == "--input") a.input = v;
		else if (k == "--output") a.output = v;
		else if (k == "--count") a.count = std::stoi(v);
		else if (k == "--timeout") a.timeout = std::stod(v);
		else if (k == "--source-id") a.source_id = v;
		else if (k == "--buflen") a.buflen = std::stoi(v);
		else if (k == "--chunklen") a.chunklen = std::stoi(v);
		else if (k == "--pull-delay-ms") a.pull_delay_ms = std::stoi(v);
		else if (k == "--push-delay-ms") a.push_delay_ms = std::stoi(v);
		else if (k == "--recover") a.recover = (v == std::string("1"));
		else if (k == "--consumers") a.consumers = std::stoi(v);
		else if (k == "--pushthrough") a.pushthrough = (v == std::string("1"));
		else if (k == "--type") a.stream_type = v;
		else if (k == "--postproc") a.postproc = std::stoi(v);
		else if (k == "--chunk") a.chunk = std::stoi(v);
		else if (k == "--zero-stamps") a.zero_stamps = (v == std::string("1"));
		else if (k == "--sync") a.sync = (v == std::string("1"));
		else if (k == "--desc") a.desc = (v == std::string("1"));
		else if (k == "--desc-output") a.desc_output = v;
		else throw std::runtime_error("unknown option " + k);
	}
	return a;
}

lsl::channel_format_t format_of(const std::string &f) {
	if (f == "float32") return lsl::cf_float32;
	if (f == "double64") return lsl::cf_double64;
	if (f == "int32") return lsl::cf_int32;
	if (f == "int16") return lsl::cf_int16;
	if (f == "int8") return lsl::cf_int8;
	if (f == "int64") return lsl::cf_int64;
	if (f == "string") return lsl::cf_string;
	throw std::runtime_error("unknown format " + f);
}

// One sample, as the file holds it: a timestamp and one token per channel.
struct Row {
	double timestamp = 0;
	std::vector<std::string> tokens;
};

std::vector<Row> read_rows(const std::string &path) {
	std::ifstream in(path);
	if (!in) throw std::runtime_error("cannot read " + path);
	std::vector<Row> rows;
	std::string line;
	while (std::getline(in, line)) {
		if (line.empty()) continue;
		std::istringstream is(line);
		std::string t;
		is >> t;
		Row r;
		r.timestamp = bits_to_double(unhex(t));
		std::string tok;
		while (is >> tok) r.tokens.push_back(tok);
		rows.push_back(std::move(r));
	}
	return rows;
}

// === outlet ===

int g_push_delay_ms = 4;
bool g_pushthrough = true;
// Push every sample with a timestamp of zero, which means the current clock.
bool g_zero_stamps = false;

template <typename T>
void push_numeric(lsl::stream_outlet &out, const std::vector<Row> &rows, int channels,
	bool is_float) {
	std::vector<T> buf(static_cast<std::size_t>(channels));
	for (const Row &r : rows) {
		for (int k = 0; k < channels; k++) {
			uint64_t bits = unhex(r.tokens[static_cast<std::size_t>(k)]);
			if (is_float) {
				if (sizeof(T) == 4) {
					uint32_t b32 = static_cast<uint32_t>(bits);
					T v;
					std::memcpy(&v, &b32, sizeof(v));
					buf[static_cast<std::size_t>(k)] = v;
				} else {
					T v;
					std::memcpy(&v, &bits, sizeof(v));
					buf[static_cast<std::size_t>(k)] = v;
				}
			} else {
				buf[static_cast<std::size_t>(k)] = static_cast<T>(static_cast<int64_t>(bits));
			}
		}
		out.push_sample(buf.data(), g_zero_stamps ? 0.0 : r.timestamp, g_pushthrough);
		if (g_push_delay_ms > 0)
			std::this_thread::sleep_for(std::chrono::milliseconds(g_push_delay_ms));
	}
}

// Push the rows in chunks. SPEC.md 9.2: the timestamp names the last sample of
// the chunk, so the caller passes the time of that sample and liblsl counts
// backward for the rest.
template <typename T>
void push_chunks_numeric(lsl::stream_outlet &out, const std::vector<Row> &rows, int channels,
	bool is_float, int chunk) {
	std::size_t n = rows.size();
	for (std::size_t start = 0; start < n; start += static_cast<std::size_t>(chunk)) {
		std::size_t count = std::min(static_cast<std::size_t>(chunk), n - start);
		std::vector<T> flat(count * static_cast<std::size_t>(channels));
		for (std::size_t k = 0; k < count; k++) {
			const Row &r = rows[start + k];
			for (int c = 0; c < channels; c++) {
				uint64_t bits = unhex(r.tokens[static_cast<std::size_t>(c)]);
				T v;
				if (is_float) {
					if (sizeof(T) == 4) {
						uint32_t b32 = static_cast<uint32_t>(bits);
						std::memcpy(&v, &b32, sizeof(v));
					} else {
						std::memcpy(&v, &bits, sizeof(v));
					}
				} else {
					v = static_cast<T>(static_cast<int64_t>(bits));
				}
				flat[k * static_cast<std::size_t>(channels) + static_cast<std::size_t>(c)] = v;
			}
		}
		// The time of the last sample of this chunk.
		double stamp = rows[start + count - 1].timestamp;
		out.push_chunk_multiplexed(flat.data(), flat.size(), stamp, true);
		if (g_push_delay_ms > 0)
			std::this_thread::sleep_for(std::chrono::milliseconds(g_push_delay_ms));
	}
}

// The description tree that the harness expects.
//
// The short description that a discovery answer carries always holds an empty
// `<desc />` (src/stream_info_impl.cpp:160). Only the whole document carries
// the tree, and that travels on the data port.
//
// `blank` and `hollow` are here on purpose. A tag with an empty text child
// writes as `<blank></blank>` and a tag with no child at all writes as
// `<hollow />`. An implementation that keeps only a text field cannot tell
// them apart.
void build_desc(lsl::stream_info &info, int channels) {
	lsl::xml_element chns = info.desc().append_child("channels");
	for (int i = 0; i < channels; i++) {
		lsl::xml_element c = chns.append_child("channel");
		c.append_child_value("label", "Ch" + std::to_string(i));
		c.append_child_value("unit", "microvolts");
		c.append_child_value("type", "EEG");
	}
	info.desc().append_child_value("manufacturer", "Acme & Co <test>");
	info.desc().append_child_value("blank", "");
	info.desc().append_child("hollow");
}

// Cut the `<desc>` part out of a whole document.
//
// Everything else in the document carries a port, a clock reading, or a random
// identifier, and none of those can be compared between two runs.
std::string desc_block(const std::string &xml) {
	auto a = xml.find("\t<desc");
	if (a == std::string::npos) return "(no desc)\n";
	auto b = xml.find("</desc>", a);
	if (b == std::string::npos) {
		b = xml.find("\n", a);
		return b == std::string::npos ? xml.substr(a) : xml.substr(a, b - a + 1);
	}
	return xml.substr(a, b - a + 8);
}

int run_outlet(const Args &a) {
	lsl::channel_format_t cf = format_of(a.format);
	std::vector<Row> rows = read_rows(a.input);

	std::string sid = a.source_id.empty() ? (a.name + "_src") : a.source_id;
	lsl::stream_info info(a.name, a.stream_type, a.channels, a.srate, cf, sid);
	if (a.desc) build_desc(info, a.channels);
	// `transp_sync_blocking` writes each push before it returns, with no queue
	// between the caller and the socket.
	lsl::stream_outlet out(info, 0, 3600,
		a.sync ? transp_sync_blocking : transp_default);

	g_push_delay_ms = a.push_delay_ms;
	g_zero_stamps = a.zero_stamps;
	g_pushthrough = a.pushthrough;
	// The outlet assigns the identifier to its own copy of the description.
	// The local copy never receives one.
	std::cerr << "READY " << out.info().uid() << "\n" << std::flush;

	// Wait for the harness to say that the inlet is connected.
	std::string go;
	if (!std::getline(std::cin, go)) return 1;

	if (!out.wait_for_consumers(static_cast<int>(a.timeout))) {
		std::cerr << "ERROR no consumer\n";
		return 1;
	}
	std::this_thread::sleep_for(std::chrono::milliseconds(200));

	if (a.chunk > 0) {
		switch (cf) {
		case lsl::cf_float32: push_chunks_numeric<float>(out, rows, a.channels, true, a.chunk); break;
		case lsl::cf_double64: push_chunks_numeric<double>(out, rows, a.channels, true, a.chunk); break;
		case lsl::cf_int32: push_chunks_numeric<int32_t>(out, rows, a.channels, false, a.chunk); break;
		case lsl::cf_int16: push_chunks_numeric<int16_t>(out, rows, a.channels, false, a.chunk); break;
		case lsl::cf_int8: push_chunks_numeric<char>(out, rows, a.channels, false, a.chunk); break;
		case lsl::cf_int64: push_chunks_numeric<int64_t>(out, rows, a.channels, false, a.chunk); break;
		default:
			std::cerr << "ERROR chunk mode covers the numeric formats only\n";
			return 2;
		}
		std::cerr << "PUSHED " << rows.size() << "\n" << std::flush;
		std::this_thread::sleep_for(std::chrono::seconds(30));
		return 0;
	}

	switch (cf) {
	case lsl::cf_float32: push_numeric<float>(out, rows, a.channels, true); break;
	case lsl::cf_double64: push_numeric<double>(out, rows, a.channels, true); break;
	case lsl::cf_int32: push_numeric<int32_t>(out, rows, a.channels, false); break;
	case lsl::cf_int16: push_numeric<int16_t>(out, rows, a.channels, false); break;
	case lsl::cf_int8: push_numeric<char>(out, rows, a.channels, false); break;
	case lsl::cf_int64: push_numeric<int64_t>(out, rows, a.channels, false); break;
	case lsl::cf_string: {
		std::vector<std::string> buf(static_cast<std::size_t>(a.channels));
		for (const Row &r : rows) {
			for (int k = 0; k < a.channels; k++)
				buf[static_cast<std::size_t>(k)] =
					unhex_bytes(r.tokens[static_cast<std::size_t>(k)]);
			out.push_sample(buf.data(), g_zero_stamps ? 0.0 : r.timestamp, g_pushthrough);
			if (g_push_delay_ms > 0)
				std::this_thread::sleep_for(std::chrono::milliseconds(g_push_delay_ms));
		}
		break;
	}
	default: return 2;
	}

	std::cerr << "PUSHED " << rows.size() << "\n" << std::flush;
	// Stay alive until the harness stops this process.
	std::this_thread::sleep_for(std::chrono::seconds(30));
	return 0;
}

// === inlet ===

int g_pull_delay_ms = 0;

template <typename T>
void pull_numeric(lsl::stream_inlet &in, std::ofstream &out, int channels, int count,
	double timeout, bool is_float) {
	std::vector<T> buf(static_cast<std::size_t>(channels));
	for (int i = 0; i < count; i++) {
		double ts = in.pull_sample(buf, timeout);
		if (ts == 0.0) break; // the pull timed out
		out << hex(double_to_bits(ts));
		for (int k = 0; k < channels; k++) {
			uint64_t bits = 0;
			T v = buf[static_cast<std::size_t>(k)];
			if (is_float) {
				std::memcpy(&bits, &v, sizeof(v));
			} else {
				bits = static_cast<uint64_t>(static_cast<int64_t>(v));
			}
			out << " " << hex(bits);
		}
		out << "\n";
		out.flush();
		if (g_pull_delay_ms > 0)
			std::this_thread::sleep_for(std::chrono::milliseconds(g_pull_delay_ms));
	}
}

int run_inlet(const Args &a) {
	lsl::channel_format_t cf = format_of(a.format);

	g_pull_delay_ms = a.pull_delay_ms;
	std::vector<lsl::stream_info> found =
		lsl::resolve_stream("name", a.name, 1, a.timeout);
	if (found.empty()) {
		std::cerr << "ERROR could not resolve " << a.name << "\n";
		return 1;
	}

	lsl::stream_inlet in(found[0], a.buflen, a.chunklen, a.recover);
	// Every stage of the post-processing must stay off. The harness compares
	// the timestamp that the sender wrote, and each stage changes it.
	in.set_postprocessing(static_cast<uint32_t>(a.postproc));
	if (a.postproc != 0) {
		// A corrected timestamp needs a measurement first. Without one the
		// clock sync stage adds zero.
		try {
			in.time_correction(5.0);
		} catch (std::exception &) {
			// No measurement inside the timeout. The stage then adds zero,
			// and the comparison sees that.
		}
	}
	in.open_stream(a.timeout);

	// The tree does not travel with a discovery answer. Ask the data port.
	// `src/info_receiver.cpp:60`.
	if (a.desc && !a.desc_output.empty()) {
		lsl::stream_info full = in.info(a.timeout);
		std::ofstream d(a.desc_output, std::ios::binary);
		d << desc_block(full.as_xml());
	}

	std::cerr << "READY " << found[0].uid() << "\n" << std::flush;

	std::ofstream out(a.output);
	if (!out) {
		std::cerr << "ERROR cannot write " << a.output << "\n";
		return 1;
	}

	switch (cf) {
	case lsl::cf_float32: pull_numeric<float>(in, out, a.channels, a.count, a.timeout, true); break;
	case lsl::cf_double64:
		pull_numeric<double>(in, out, a.channels, a.count, a.timeout, true);
		break;
	case lsl::cf_int32: pull_numeric<int32_t>(in, out, a.channels, a.count, a.timeout, false); break;
	case lsl::cf_int16: pull_numeric<int16_t>(in, out, a.channels, a.count, a.timeout, false); break;
	case lsl::cf_int8: pull_numeric<char>(in, out, a.channels, a.count, a.timeout, false); break;
	case lsl::cf_int64: pull_numeric<int64_t>(in, out, a.channels, a.count, a.timeout, false); break;
	case lsl::cf_string: {
		std::vector<std::string> buf(static_cast<std::size_t>(a.channels));
		for (int i = 0; i < a.count; i++) {
			double ts = in.pull_sample(buf, a.timeout);
			if (ts == 0.0) break;
			out << hex(double_to_bits(ts));
			for (int k = 0; k < a.channels; k++) {
				std::string h = hex_bytes(buf[static_cast<std::size_t>(k)]);
				out << " " << (h.empty() ? std::string("-") : h);
			}
			out << "\n";
		}
		break;
	}
	default: return 2;
	}

	out.flush();
	std::cerr << "DONE\n" << std::flush;
	return 0;
}

} // namespace

int main(int argc, char **argv) {
	try {
		Args a = parse(argc, argv);
		if (a.mode == "outlet") return run_outlet(a);
		if (a.mode == "inlet") return run_inlet(a);
		std::cerr << "unknown mode " << a.mode << "\n";
		return 2;
	} catch (std::exception &e) {
		std::cerr << "ERROR " << e.what() << "\n";
		return 1;
	}
}
