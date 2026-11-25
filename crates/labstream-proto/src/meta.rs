//! In-band live metadata records for the `meta/1` feature (`docs/extensions.md`,
//! section `meta`, `docs/roadmap.md`, step 16).
//!
//! A metadata record stands between two samples on the TCP feed channel. It
//! carries the outlet sample sequence number at the point of emission, a full
//! outlet-clock timestamp, a UTF-8 key, and a UTF-8 value.
//!
//! Wire layout (little-endian):
//! ```text
//! u8   tag        0xFC
//! u64  seq        outlet sequence number at the point of emission
//! f64  timestamp  outlet clock timestamp in seconds
//! u32  key_len    byte length of key (UTF-8)
//! u32  val_len    byte length of value (UTF-8)
//! [u8; key_len]   key bytes
//! [u8; val_len]   value bytes
//! ```

/// First byte of a live metadata record on the TCP feed channel.
///
/// Sample tags are `0x01`, `0x02`, `0xF9`, and `0xFA`. Control tags are
/// `0xFD` (`dgram` path) and `0xFE` (`seq`). `0xFC` is distinct from all of
/// them.
pub const TAG: u8 = 0xFC;

/// Fixed header length in bytes before the variable-length key and value.
pub const HEADER_LEN: usize = 1 + 8 + 8 + 4 + 4;

/// Maximum allowed byte length of either key or value in one metadata record.
pub const MAX_FIELD_LEN: usize = 64 * 1024;

/// One live metadata change emitted by an outlet.
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataUpdate {
    /// Outlet sample sequence number at the moment the change was emitted.
    ///
    /// Samples with sequence numbers below `seq` came before this change.
    /// Samples with sequence numbers at or above `seq` came after it.
    pub seq: u64,
    /// Timestamp of the change on the stream clock, in seconds.
    pub timestamp: f64,
    /// Hierarchical or flat key name (for example `impedance/Fp1`).
    pub key: String,
    /// Value string (for example `4.2 kOhm` or `bad`).
    pub value: String,
}

/// Append one `meta/1` record to `out`.
pub fn write_record(out: &mut Vec<u8>, update: &MetadataUpdate) {
    let key = update.key.as_bytes();
    let val = update.value.as_bytes();
    let key_len = key.len().min(MAX_FIELD_LEN);
    let val_len = val.len().min(MAX_FIELD_LEN);
    out.reserve(HEADER_LEN + key_len + val_len);
    out.push(TAG);
    out.extend_from_slice(&update.seq.to_le_bytes());
    out.extend_from_slice(&update.timestamp.to_le_bytes());
    out.extend_from_slice(&(key_len as u32).to_le_bytes());
    out.extend_from_slice(&(val_len as u32).to_le_bytes());
    out.extend_from_slice(&key[..key_len]);
    out.extend_from_slice(&val[..val_len]);
}

/// Result of inspecting the start of a TCP feed buffer for a `meta/1` record.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// A complete record and the total number of bytes consumed.
    Record(MetadataUpdate, usize),
    /// A `meta/1` record that is cut before its end. The caller reads more bytes.
    Incomplete,
    /// Not a `meta/1` record (starts with another tag or empty slice).
    Other,
    /// Malformed `meta/1` record (oversized field or invalid UTF-8).
    Invalid,
}

/// Inspect the start of `buf` for a `meta/1` record.
pub fn peek(buf: &[u8]) -> Next {
    match buf.first() {
        Some(&TAG) => {}
        _ => return Next::Other,
    }
    if buf.len() < HEADER_LEN {
        return Next::Incomplete;
    }
    let mut seq_b = [0u8; 8];
    seq_b.copy_from_slice(&buf[1..9]);
    let seq = u64::from_le_bytes(seq_b);

    let mut ts_b = [0u8; 8];
    ts_b.copy_from_slice(&buf[9..17]);
    let timestamp = f64::from_le_bytes(ts_b);

    let mut kl_b = [0u8; 4];
    kl_b.copy_from_slice(&buf[17..21]);
    let key_len = u32::from_le_bytes(kl_b) as usize;

    let mut vl_b = [0u8; 4];
    vl_b.copy_from_slice(&buf[21..25]);
    let val_len = u32::from_le_bytes(vl_b) as usize;

    if key_len > MAX_FIELD_LEN || val_len > MAX_FIELD_LEN {
        return Next::Invalid;
    }
    let total = match HEADER_LEN
        .checked_add(key_len)
        .and_then(|n| n.checked_add(val_len))
    {
        Some(n) => n,
        None => return Next::Invalid,
    };
    if buf.len() < total {
        return Next::Incomplete;
    }
    let key = match core::str::from_utf8(&buf[HEADER_LEN..HEADER_LEN + key_len]) {
        Ok(s) => s.to_string(),
        Err(_) => return Next::Invalid,
    };
    let value = match core::str::from_utf8(&buf[HEADER_LEN + key_len..total]) {
        Ok(s) => s.to_string(),
        Err(_) => return Next::Invalid,
    };
    Next::Record(
        MetadataUpdate {
            seq,
            timestamp,
            key,
            value,
        },
        total,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_fields_and_exact_timestamp_bits() {
        let orig = MetadataUpdate {
            seq: 42_000,
            timestamp: 12345.6789012345,
            key: "impedance/Fp1".to_string(),
            value: "4.2 kOhm".to_string(),
        };
        let mut buf = Vec::new();
        write_record(&mut buf, &orig);
        assert_eq!(buf[0], TAG);
        match peek(&buf) {
            Next::Record(got, used) => {
                assert_eq!(used, buf.len());
                assert_eq!(got.seq, orig.seq);
                assert_eq!(got.timestamp.to_bits(), orig.timestamp.to_bits());
                assert_eq!(got.key, orig.key);
                assert_eq!(got.value, orig.value);
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn every_truncation_returns_incomplete() {
        let orig = MetadataUpdate {
            seq: 7,
            timestamp: 1.5,
            key: "ch/0/bad".to_string(),
            value: "1".to_string(),
        };
        let mut buf = Vec::new();
        write_record(&mut buf, &orig);
        for cut in 1..buf.len() {
            assert_eq!(peek(&buf[..cut]), Next::Incomplete, "cut at {cut}");
        }
    }

    #[test]
    fn other_tags_and_empty_return_other() {
        assert_eq!(peek(&[]), Next::Other);
        assert_eq!(peek(&[0x01, 0x00]), Next::Other);
        assert_eq!(peek(&[0x02, 0x00]), Next::Other);
        assert_eq!(peek(&[0xFE, 0x00]), Next::Other);
    }

    #[test]
    fn oversized_and_bad_utf8_return_invalid() {
        let mut bad_len = vec![TAG];
        bad_len.extend_from_slice(&0u64.to_le_bytes());
        bad_len.extend_from_slice(&0.0f64.to_le_bytes());
        bad_len.extend_from_slice(&((MAX_FIELD_LEN as u32) + 1).to_le_bytes());
        bad_len.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(peek(&bad_len), Next::Invalid);

        let mut bad_utf8 = vec![TAG];
        bad_utf8.extend_from_slice(&0u64.to_le_bytes());
        bad_utf8.extend_from_slice(&0.0f64.to_le_bytes());
        bad_utf8.extend_from_slice(&2u32.to_le_bytes());
        bad_utf8.extend_from_slice(&0u32.to_le_bytes());
        bad_utf8.extend_from_slice(&[0xFF, 0xFE]);
        assert_eq!(peek(&bad_utf8), Next::Invalid);
    }
}
