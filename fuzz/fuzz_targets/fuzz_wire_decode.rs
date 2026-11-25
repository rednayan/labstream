#![no_main]
use labstream_wire::{ByteOrder, Codec, Format};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let fmt = match data[0] % 7 {
        0 => Format::Float32,
        1 => Format::Double64,
        2 => Format::String,
        3 => Format::Int32,
        4 => Format::Int16,
        5 => Format::Int8,
        _ => Format::Int64,
    };
    let channels = ((data[1] as usize) % 16).max(1);
    let order = if data[2] & 1 == 0 {
        ByteOrder::Little
    } else {
        ByteOrder::Big
    };
    let codec = Codec::new(fmt, channels, order);
    let _ = codec.decode(&data[3..]);
});
