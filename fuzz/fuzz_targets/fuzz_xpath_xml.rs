#![no_main]
use labstream_net::StreamInfo;
use labstream_wire::Format;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = StreamInfo::from_fullinfo_xml(text);
    let info = StreamInfo::new("FuzzStream", "EEG", 8, Format::Float32, 250.0);
    let _ = info.matches(text);
});
