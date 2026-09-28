// Golden sequence generator for the timestamp post-processor.
//
// This program drives the real liblsl filter directly. It compiles
// `src/time_postprocessor.cpp` from the pinned source tree, so the numbers
// come from the same code that a real inlet runs.
//
// The filter is deterministic for a given input sequence. That is what makes a
// golden sequence possible at all, and it is why M6 can compare exact numbers
// where M4 and M5 could only compare a stream that both sides agreed on.
//
// Every number crosses the boundary as a bit pattern, so no text formatting of
// a float takes place.
//
// Usage: gentime <case> <count>
//   ramp        a clean ramp with no jitter
//   jitter      a ramp with a fixed pseudo-random wobble
//   gap         a ramp with one long gap in the middle
//   irregular   timestamps that do not follow a rate
//   backward    a ramp that steps backward twice

#include "time_postprocessor.h"

#include <cmath>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <string>
#include <vector>

namespace {

uint64_t bits(double d) {
	uint64_t b;
	std::memcpy(&b, &d, sizeof(b));
	return b;
}

// A small fixed generator. A library generator differs between platforms, and
// the sequence must be the same everywhere.
struct Rng {
	uint64_t s = 0x9E3779B97F4A7C15ull;
	double next() {
		s ^= s << 13;
		s ^= s >> 7;
		s ^= s << 17;
		// A value in [-1, 1).
		return static_cast<double>(static_cast<int64_t>(s >> 11)) / 4503599627370496.0 - 1.0;
	}
};

std::vector<double> build(const std::string &name, int n, double srate, double t0) {
	std::vector<double> out;
	out.reserve(static_cast<std::size_t>(n));
	Rng rng;
	for (int i = 0; i < n; i++) {
		double t = t0 + i / srate;
		if (name == "ramp") {
			out.push_back(t);
		} else if (name == "jitter") {
			out.push_back(t + 0.05 + rng.next() * 0.005);
		} else if (name == "gap") {
			// One long gap in the middle, with no call to skip_samples. The
			// filter must recover on its own.
			out.push_back(i < n / 2 ? t : t + 3.0);
		} else if (name == "irregular") {
			out.push_back(t0 + std::fabs(rng.next()) * i);
		} else if (name == "backward") {
			double v = t;
			if (i == n / 3) v = t - 1.0;
			if (i == 2 * n / 3) v = t - 0.25;
			out.push_back(v);
		} else {
			out.push_back(t);
		}
	}
	return out;
}

} // namespace

int main(int argc, char **argv) {
	if (argc < 3) {
		std::cerr << "usage: gentime <case> <count> [srate] [t0]\n";
		return 2;
	}
	std::string name = argv[1];
	int n = std::stoi(argv[2]);
	// A caller can vary the rate and the first timestamp. Both change the
	// numbers that the filter sees, so both belong in the sequence set.
	double srate = argc > 3 ? std::stod(argv[3]) : 100.0;
	double t0 = argc > 4 ? std::stod(argv[4]) : 5000.0;
	const double halftime = 90.0;

	std::vector<double> input = build(name, n, srate, t0);

	// Drive the filter exactly as an inlet does: build it from the first
	// timestamp, then feed every timestamp including that first one
	// (`src/time_postprocessor.cpp:93-98`).
	lsl::postproc_dejitterer pp(input[0], srate, halftime);

	std::cout << "{\n \"case\": \"" << name << "\",\n"
			  << " \"count\": " << n << ",\n"
			  << " \"srate_bits\": \"" << std::hex << bits(srate) << std::dec << "\",\n"
			  << " \"halftime_bits\": \"" << std::hex << bits(halftime) << std::dec << "\",\n"
			  << " \"samples\": [\n";
	for (int i = 0; i < n; i++) {
		double got = pp.dejitter(input[i]);
		std::cout << "  [\"" << std::hex << bits(input[i]) << "\", \"" << bits(got) << std::dec
				  << "\"]";
		if (i + 1 < n) std::cout << ",";
		std::cout << "\n";
	}
	std::cout << " ],\n";
	std::cout << " \"final\": {\"w0\": \"" << std::hex << bits(pp.w0_) << "\", \"w1\": \""
			  << bits(pp.w1_) << "\", \"p00\": \"" << bits(pp.P00_) << "\", \"p01\": \""
			  << bits(pp.P01_) << "\", \"p11\": \"" << bits(pp.P11_) << "\", \"lam\": \""
			  << bits(pp.lam_) << std::dec << "\", \"t0\": " << (unsigned long)pp.t0_
			  << ", \"samples_since_t0\": " << (unsigned long)pp.samples_since_t0_ << "}\n";
	std::cout << "}\n";
	return 0;
}
