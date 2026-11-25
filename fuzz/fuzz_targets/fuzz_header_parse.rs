#![no_main]
use labstream_proto::header;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = header::parse_block(data);
});
