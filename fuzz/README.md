# Fuzz targets for labstream

This package holds six `cargo-fuzz` (libFuzzer) targets that feed arbitrary
bytes into every untrusted wire and text parser in `labstream`:

| Target | Parser under test |
|---|---|
| `fuzz_wire_decode` | `labstream_wire::Codec::decode` across all 7 formats and byte orders |
| `fuzz_comp_decode` | `labstream_proto::comp` TCP and UDP Golomb-Rice decoders |
| `fuzz_dgram_decode` | `labstream_proto::dgram::{parse, peek}` |
| `fuzz_meta_decode` | `labstream_proto::meta::peek` (`0xFC` records) |
| `fuzz_xpath_xml` | `StreamInfo::from_fullinfo_xml` and XPath 1.0 `matches` |
| `fuzz_header_parse` | `labstream_proto::header::parse_block` |

## Run locally

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run fuzz_wire_decode -- -max_total_time=60
```

GitHub Actions runs all six targets weekly in `.github/workflows/security.yml`
alongside Miri, AddressSanitizer, and ThreadSanitizer.
