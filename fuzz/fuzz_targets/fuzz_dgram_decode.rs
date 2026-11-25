#![no_main]
use labstream_proto::dgram;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = dgram::parse(data);
    let _ = dgram::peek(data);
});
