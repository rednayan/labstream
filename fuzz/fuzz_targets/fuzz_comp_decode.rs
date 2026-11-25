#![no_main]
use labstream_proto::comp::{decode_dgram_payload, CompState};
use labstream_wire::{ByteOrder, Codec, Format};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let fmt = match data[0] % 6 {
        0 => Format::Float32,
        1 => Format::Double64,
        2 => Format::Int32,
        3 => Format::Int16,
        4 => Format::Int8,
        _ => Format::Int64,
    };
    let channels = ((data[1] as usize) % 16).max(1);
    let count = ((data[2] as usize) % 16).max(1);
    let payload = &data[3..];
    let codec = Codec::new(fmt, channels, ByteOrder::Little);

    let mut state = CompState::new(fmt, channels);
    let _ = state.decode_tcp_sample(&codec, payload);

    let mut out = Vec::new();
    let _ = decode_dgram_payload(&codec, count, payload, &mut out);
});
