// Golden vector generator for the LSL conformance suite.
//
// This program starts one outlet, waits for a consumer, and pushes a fixed
// sequence of samples. The capture rig records the bytes that liblsl writes.
// Those bytes are the golden vectors.
//
// The program also writes the values that it pushed to standard output, as
// JSON. That record is ground truth from the producer side. A test that only
// compared bytes can pass when a decoder and an encoder hold the same error.
// The value record closes that hole.
//
// Every value appears as a bit pattern or as an exact integer, so no text
// formatting of a float takes place.
//
// The program uses the public liblsl API only.
//
// Usage: genvectors <format> <channels> <name>

#include <lsl_cpp.h>

#include <chrono>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <limits>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace {

const double DEDUCED = -1.0;

std::string hex_u64(uint64_t v) {
	std::ostringstream os;
	os << "0x" << std::hex << v;
	return os.str();
}

template <typename T> uint64_t bits_of(T v) {
	uint64_t out = 0;
	std::memcpy(&out, &v, sizeof(T));
	return out;
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

// Collects the record of what the program pushed.
struct Recorder {
	std::ostringstream body;
	bool first = true;

	void begin_sample(double t) {
		if (!first) body << ",\n";
		first = false;
		body << "  {\"t_bits\": \"" << hex_u64(bits_of(t)) << "\", \"values\": [";
	}
	void end_sample() { body << "]}"; }
	void sep(int k) {
		if (k) body << ", ";
	}
};

template <typename T>
void push_numeric(lsl::stream_outlet &out, int channels, const std::vector<T> &pool,
	Recorder &rec, bool is_float) {
	std::vector<T> buf(static_cast<std::size_t>(channels));

	// The timestamp list covers the deduced tag and several transmitted values.
	std::vector<double> stamps = {DEDUCED, 1.5, -2.25, 7.125};
	for (std::size_t i = 0; i < pool.size(); i++) stamps.push_back(100.0 + static_cast<double>(i));

	for (std::size_t i = 0; i < stamps.size(); i++) {
		for (int k = 0; k < channels; k++)
			buf[static_cast<std::size_t>(k)] = pool[(i + static_cast<std::size_t>(k)) % pool.size()];

		rec.begin_sample(stamps[i]);
		for (int k = 0; k < channels; k++) {
			rec.sep(k);
			T v = buf[static_cast<std::size_t>(k)];
			if (is_float)
				rec.body << "\"" << hex_u64(bits_of(v)) << "\"";
			else
				rec.body << "\"" << static_cast<long long>(v) << "\"";
		}
		rec.end_sample();

		out.push_sample(buf.data(), stamps[i], true);
		std::this_thread::sleep_for(std::chrono::milliseconds(15));
	}
}

void push_strings(lsl::stream_outlet &out, int channels, Recorder &rec) {
	// These cases cover both length prefix width boundaries. A length of 255
	// writes the width 1. A length of 256 writes the width 4.
	std::vector<std::string> pool = {
		std::string(""),
		std::string("a"),
		std::string("hello world"),
		std::string(255, 'x'),
		std::string(256, 'y'),
		std::string(257, 'z'),
		std::string("nul\0inside", 10),
		std::string("\xff\xfe\x01\x02", 4),
	};

	std::vector<std::string> buf(static_cast<std::size_t>(channels));
	std::vector<double> stamps = {DEDUCED, 1.5, -2.25, 7.125};
	for (std::size_t i = 0; i < pool.size(); i++) stamps.push_back(100.0 + static_cast<double>(i));

	for (std::size_t i = 0; i < stamps.size(); i++) {
		for (int k = 0; k < channels; k++)
			buf[static_cast<std::size_t>(k)] = pool[(i + static_cast<std::size_t>(k)) % pool.size()];

		rec.begin_sample(stamps[i]);
		for (int k = 0; k < channels; k++) {
			rec.sep(k);
			rec.body << "\"" << hex_bytes(buf[static_cast<std::size_t>(k)]) << "\"";
		}
		rec.end_sample();

		out.push_sample(buf.data(), stamps[i], true);
		std::this_thread::sleep_for(std::chrono::milliseconds(15));
	}
}

} // namespace

int main(int argc, char **argv) {
	if (argc < 4) {
		std::cerr << "usage: genvectors <format> <channels> <name>\n";
		return 2;
	}
	std::string fmt = argv[1];
	int channels = std::stoi(argv[2]);
	std::string name = argv[3];

	lsl::channel_format_t cf = lsl::cf_undefined;
	if (fmt == "float32") cf = lsl::cf_float32;
	else if (fmt == "double64") cf = lsl::cf_double64;
	else if (fmt == "string") cf = lsl::cf_string;
	else if (fmt == "int8") cf = lsl::cf_int8;
	else if (fmt == "int16") cf = lsl::cf_int16;
	else if (fmt == "int32") cf = lsl::cf_int32;
	else if (fmt == "int64") cf = lsl::cf_int64;
	else {
		std::cerr << "unknown format: " << fmt << "\n";
		return 2;
	}

	lsl::stream_info info(name, "Vectors", channels, 100.0, cf, name + "_src");
	lsl::stream_outlet out(info, 0, 360);

	std::cerr << "OUTLET_READY\n" << std::flush;

	if (!out.wait_for_consumers(30)) {
		std::cerr << "no consumer arrived\n";
		return 1;
	}
	std::this_thread::sleep_for(std::chrono::milliseconds(400));

	Recorder rec;
	switch (cf) {
	case lsl::cf_float32: {
		std::vector<float> pool = {0.0f, -0.0f, 1.0f, -1.0f,
			std::numeric_limits<float>::denorm_min(), std::numeric_limits<float>::min(),
			std::numeric_limits<float>::max(), -std::numeric_limits<float>::max(),
			std::numeric_limits<float>::infinity(), -std::numeric_limits<float>::infinity(),
			std::numeric_limits<float>::quiet_NaN()};
		push_numeric(out, channels, pool, rec, true);
		break;
	}
	case lsl::cf_double64: {
		std::vector<double> pool = {0.0, -0.0, 1.0, -1.0,
			std::numeric_limits<double>::denorm_min(), std::numeric_limits<double>::min(),
			std::numeric_limits<double>::max(), -std::numeric_limits<double>::max(),
			std::numeric_limits<double>::infinity(), -std::numeric_limits<double>::infinity(),
			std::numeric_limits<double>::quiet_NaN()};
		push_numeric(out, channels, pool, rec, true);
		break;
	}
	case lsl::cf_int8: {
		std::vector<char> pool = {0, 1, -1, 127, -128, 42};
		push_numeric(out, channels, pool, rec, false);
		break;
	}
	case lsl::cf_int16: {
		std::vector<int16_t> pool = {0, 1, -1, 32767, -32768, 258};
		push_numeric(out, channels, pool, rec, false);
		break;
	}
	case lsl::cf_int32: {
		std::vector<int32_t> pool = {0, 1, -1, 2147483647, -2147483647 - 1, 66051};
		push_numeric(out, channels, pool, rec, false);
		break;
	}
	case lsl::cf_int64: {
		std::vector<int64_t> pool = {0, 1, -1, 9223372036854775807LL,
			-9223372036854775807LL - 1, 283686952306183LL};
		push_numeric(out, channels, pool, rec, false);
		break;
	}
	case lsl::cf_string: push_strings(out, channels, rec); break;
	default: return 2;
	}

	// The value record goes to standard output as JSON.
	std::cout << "{\n \"format\": \"" << fmt << "\",\n \"channels\": " << channels
			  << ",\n \"pushed\": [\n" << rec.body.str() << "\n ]\n}\n" << std::flush;

	std::cerr << "PUSHED\n" << std::flush;
	std::this_thread::sleep_for(std::chrono::seconds(2));
	return 0;
}
