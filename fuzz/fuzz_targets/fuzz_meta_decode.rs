#![no_main]
use labstream_proto::meta;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = meta::peek(data);
});
