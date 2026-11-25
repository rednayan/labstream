//! Opt-in negotiated transport encryption (`enc/1`, `docs/roadmap.md`, step 14).
//!
//! Uses published standards implemented by the reviewed `ring` crate (no custom
//! cryptography):
//! - Ephemeral X25519 key agreement (RFC 7748) carried in `Labstream-Features:
//!   enc/1=<64-hex-pubkey>` with zero extra round-trips.
//! - HKDF-SHA256 (RFC 5869) key derivation bound to both public keys and an
//!   optional pre-shared key (`labstream.EncryptionKey`) for mutual peer
//!   authentication.
//! - Length-prefixed ChaCha20-Poly1305 AEAD framing (RFC 8439) with strict
//!   64-bit monotonic nonces and frame-length AAD binding on every byte after
//!   the two plain test-pattern samples.
//!
//! `enc/1` is **off by default**. Users enable it by adding `enc` to
//! `labstream.Features`, setting `labstream.RequireEncryption = 1`, or calling
//! [`set_runtime_policy`].

use crate::config;
use labstream_proto::features;
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

/// Maximum plaintext bytes sealed into one ChaCha20-Poly1305 frame.
pub const MAX_FRAME_PLAIN: usize = 65536;

/// Poly1305 authentication tag width in bytes.
pub const TAG_LEN: usize = 16;

/// Maximum ciphertext bytes accepted in one frame (plaintext + Poly1305 tag).
pub const MAX_FRAME_CIPHER: usize = MAX_FRAME_PLAIN + TAG_LEN;

/// Process-wide encryption policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncryptionPolicy {
    /// When true, offer and accept `enc/1` during feed negotiation.
    pub enabled: bool,
    /// When true, refuse any feed connection that does not negotiate `enc/1`.
    pub required: bool,
    /// Optional pre-shared key mixed into HKDF-SHA256 for mutual authentication.
    pub psk: Option<String>,
}

impl EncryptionPolicy {
    /// Policy with encryption disabled (the default).
    pub fn off() -> Self {
        EncryptionPolicy::default()
    }

    /// Opportunistic negotiated encryption: encrypt with labstream peers that
    /// also enable `enc`, fall back to plain LSL with `liblsl` peers.
    pub fn opportunistic(psk: Option<String>) -> Self {
        EncryptionPolicy {
            enabled: true,
            required: false,
            psk,
        }
    }

    /// Strict encryption: require `enc/1` and fail with a clear error against
    /// any plain peer.
    pub fn required(psk: Option<String>) -> Self {
        EncryptionPolicy {
            enabled: true,
            required: true,
            psk,
        }
    }
}

static RUNTIME_POLICY: OnceLock<Mutex<Option<EncryptionPolicy>>> = OnceLock::new();

fn policy_lock() -> &'static Mutex<Option<EncryptionPolicy>> {
    RUNTIME_POLICY.get_or_init(|| Mutex::new(None))
}

/// Override the active encryption policy at runtime (pass `None` to revert to
/// `lsl_api.cfg` defaults).
pub fn set_runtime_policy(policy: Option<EncryptionPolicy>) {
    *policy_lock().lock().unwrap() = policy;
}

/// Active encryption policy from [`set_runtime_policy`] or `lsl_api.cfg`.
pub fn effective_policy() -> EncryptionPolicy {
    if let Some(p) = policy_lock().lock().unwrap().clone() {
        return p;
    }
    let cfg = config::get();
    let enabled = cfg.require_encryption || cfg.features.iter().any(|f| f == features::ENC);
    EncryptionPolicy {
        enabled,
        required: cfg.require_encryption,
        psk: cfg.encryption_key.clone(),
    }
}

/// True when this binary was compiled with the `encryption` Cargo feature.
pub const fn compiled_in() -> bool {
    cfg!(feature = "encryption")
}

#[cfg(feature = "encryption")]
mod imp {
    use super::*;
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
    use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
    use ring::digest::{Context, SHA256};
    use ring::hkdf::{Salt, HKDF_SHA256};
    use ring::rand::SystemRandom;

    /// Ephemeral X25519 keypair for one feed handshake.
    pub struct EphemeralKey {
        private: EphemeralPrivateKey,
        public: [u8; 32],
    }

    impl EphemeralKey {
        /// Generate a fresh X25519 keypair from the OS CSPRNG.
        pub fn generate() -> io::Result<Self> {
            let rng = SystemRandom::new();
            let private = EphemeralPrivateKey::generate(&X25519, &rng)
                .map_err(|_| io::Error::other("X25519 key generation failed"))?;
            let pub_bytes = private
                .compute_public_key()
                .map_err(|_| io::Error::other("X25519 public key computation failed"))?;
            let mut public = [0u8; 32];
            public.copy_from_slice(pub_bytes.as_ref());
            Ok(EphemeralKey { private, public })
        }

        /// 32-byte X25519 public key sent in `enc/1=<64-hex>`.
        pub fn public_bytes(&self) -> [u8; 32] {
            self.public
        }

        /// Complete X25519 key agreement and derive the outlet-to-inlet
        /// ChaCha20-Poly1305 session key via HKDF-SHA256.
        ///
        /// `inlet_pk` and `outlet_pk` are bound into the HKDF salt together
        /// with any optional pre-shared key so both sides agree on the exact
        /// transcript.
        pub fn derive_feed_key(
            self,
            peer_pk: [u8; 32],
            is_outlet: bool,
            psk: Option<&str>,
        ) -> io::Result<[u8; 32]> {
            let (inlet_pk, outlet_pk) = if is_outlet {
                (peer_pk, self.public)
            } else {
                (self.public, peer_pk)
            };
            let peer = UnparsedPublicKey::new(&X25519, peer_pk);
            agreement::agree_ephemeral(self.private, &peer, |shared| {
                let mut salt_ctx = Context::new(&SHA256);
                salt_ctx.update(b"labstream-enc-v1");
                salt_ctx.update(&inlet_pk);
                salt_ctx.update(&outlet_pk);
                if let Some(secret) = psk {
                    salt_ctx.update(b"|psk|");
                    salt_ctx.update(secret.as_bytes());
                }
                let salt_digest = salt_ctx.finish();
                let salt = Salt::new(HKDF_SHA256, salt_digest.as_ref());
                let prk = salt.extract(shared);
                let okm = prk
                    .expand(&[b"labstream-enc-v1-o2i"], HKDF_SHA256)
                    .map_err(|_| ())?;
                let mut key = [0u8; 32];
                okm.fill(&mut key).map_err(|_| ())?;
                Ok::<[u8; 32], ()>(key)
            })
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "X25519 key agreement failed"))?
            .map_err(|_| io::Error::other("HKDF-SHA256 key derivation failed"))
        }
    }

    fn make_nonce(seq: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[4..12].copy_from_slice(&seq.to_le_bytes());
        Nonce::assume_unique_for_key(n)
    }

    fn make_key(raw: &[u8; 32]) -> io::Result<LessSafeKey> {
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, raw)
            .map_err(|_| io::Error::other("invalid ChaCha20-Poly1305 key"))?;
        Ok(LessSafeKey::new(unbound))
    }

    /// Authenticated ChaCha20-Poly1305 frame writer wrapping any `Write` stream.
    pub struct EncWriter<W: Write> {
        inner: W,
        key: LessSafeKey,
        seq: u64,
        buf: Vec<u8>,
    }

    impl<W: Write> EncWriter<W> {
        /// Create an encrypted writer from a 32-byte derived session key.
        pub fn new(inner: W, key: &[u8; 32]) -> io::Result<Self> {
            Ok(EncWriter {
                inner,
                key: make_key(key)?,
                seq: 0,
                buf: Vec::with_capacity(4096),
            })
        }

        /// Seal and send a single authenticated frame (including empty
        /// key-confirmation frames).
        pub fn write_frame(&mut self, plain: &[u8]) -> io::Result<()> {
            if self.seq == u64::MAX {
                return Err(io::Error::other("encryption nonce space exhausted"));
            }
            let ct_len = plain.len() + TAG_LEN;
            let len_bytes = (ct_len as u32).to_le_bytes();
            self.buf.clear();
            self.buf.extend_from_slice(&len_bytes);
            self.buf.extend_from_slice(plain);
            let nonce = make_nonce(self.seq);
            self.seq += 1;
            let tag = self
                .key
                .seal_in_place_separate_tag(nonce, Aad::from(len_bytes), &mut self.buf[4..])
                .map_err(|_| io::Error::other("ChaCha20-Poly1305 encryption failed"))?;
            self.buf.extend_from_slice(tag.as_ref());
            self.inner.write_all(&self.buf)
        }
    }

    impl<W: Write> Write for EncWriter<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            for chunk in buf.chunks(MAX_FRAME_PLAIN) {
                self.write_frame(chunk)?;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    /// Authenticated ChaCha20-Poly1305 frame reader that decrypts ciphertext
    /// chunks into plaintext bytes.
    pub struct EncReader {
        key: LessSafeKey,
        seq: u64,
        cipher_carry: Vec<u8>,
    }

    impl EncReader {
        /// Create an encrypted reader from a 32-byte derived session key.
        pub fn new(key: &[u8; 32]) -> io::Result<Self> {
            Ok(EncReader {
                key: make_key(key)?,
                seq: 0,
                cipher_carry: Vec::with_capacity(4096),
            })
        }

        /// Feed raw ciphertext bytes from the TCP socket and append any newly
        /// authenticated plaintext bytes into `plain_out`.
        pub fn decrypt_into(&mut self, raw: &[u8], plain_out: &mut Vec<u8>) -> io::Result<()> {
            self.cipher_carry.extend_from_slice(raw);
            let mut pos = 0usize;
            while self.cipher_carry.len() - pos >= 4 {
                let len_bytes: [u8; 4] = self.cipher_carry[pos..pos + 4].try_into().unwrap();
                let ct_len = u32::from_le_bytes(len_bytes) as usize;
                if !(TAG_LEN..=MAX_FRAME_CIPHER).contains(&ct_len) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid encrypted frame length",
                    ));
                }
                if self.cipher_carry.len() - pos < 4 + ct_len {
                    break;
                }
                if self.seq == u64::MAX {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "encryption nonce space exhausted",
                    ));
                }
                let nonce = make_nonce(self.seq);
                self.seq += 1;
                let frame = &mut self.cipher_carry[pos + 4..pos + 4 + ct_len];
                let plain = self
                    .key
                    .open_in_place(nonce, Aad::from(len_bytes), frame)
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "encrypted frame authentication failed (wrong key or tampered data)",
                        )
                    })?;
                plain_out.extend_from_slice(plain);
                pos += 4 + ct_len;
            }
            if pos > 0 {
                self.cipher_carry.drain(..pos);
            }
            Ok(())
        }
    }
}

#[cfg(not(feature = "encryption"))]
mod imp {
    use super::*;

    /// Stub keypair when compiled without the `encryption` Cargo feature.
    pub struct EphemeralKey;

    impl EphemeralKey {
        /// Fails when compiled without `encryption`.
        pub fn generate() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }

        /// Never reached without `encryption`.
        pub fn public_bytes(&self) -> [u8; 32] {
            [0u8; 32]
        }

        /// Fails when compiled without `encryption`.
        pub fn derive_feed_key(
            self,
            _peer_pk: [u8; 32],
            _is_outlet: bool,
            _psk: Option<&str>,
        ) -> io::Result<[u8; 32]> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }
    }

    /// Stub writer when compiled without `encryption`.
    pub struct EncWriter<W: Write>(W);

    impl<W: Write> EncWriter<W> {
        /// Fails when compiled without `encryption`.
        pub fn new(_inner: W, _key: &[u8; 32]) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }

        /// Fails when compiled without `encryption`.
        pub fn write_frame(&mut self, _plain: &[u8]) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }
    }

    impl<W: Write> Write for EncWriter<W> {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    /// Stub reader when compiled without `encryption`.
    pub struct EncReader;

    impl EncReader {
        /// Fails when compiled without `encryption`.
        pub fn new(_key: &[u8; 32]) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }

        /// Fails when compiled without `encryption`.
        pub fn decrypt_into(&mut self, _raw: &[u8], _plain_out: &mut Vec<u8>) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "labstream was built without the encryption feature",
            ))
        }
    }
}

pub use imp::{EncReader, EncWriter, EphemeralKey};

#[cfg(all(test, feature = "encryption"))]
mod tests {
    use super::*;

    #[test]
    fn x25519_hkdf_chacha20_round_trip_hides_plaintext_and_detects_tampering() {
        let in_kp = EphemeralKey::generate().unwrap();
        let out_kp = EphemeralKey::generate().unwrap();
        let in_pk = in_kp.public_bytes();
        let out_pk = out_kp.public_bytes();

        let k_out = out_kp
            .derive_feed_key(in_pk, true, Some("lab-secret-42"))
            .unwrap();
        let k_in = in_kp
            .derive_feed_key(out_pk, false, Some("lab-secret-42"))
            .unwrap();
        assert_eq!(k_out, k_in);

        let plaintext = b"SECRET_EEG_SAMPLE_MARKER_0123456789";
        let mut wire = Vec::new();
        {
            let mut w = EncWriter::new(&mut wire, &k_out).unwrap();
            w.write_all(plaintext).unwrap();
            w.flush().unwrap();
        }
        // Criterion 1: no plaintext sample bytes on the wire.
        assert!(
            !wire.windows(plaintext.len()).any(|w| w == plaintext),
            "plaintext leaked onto wire"
        );

        // Fragmented reads decrypt cleanly.
        let mut r = EncReader::new(&k_in).unwrap();
        let mut recovered = Vec::new();
        for chunk in wire.chunks(7) {
            r.decrypt_into(chunk, &mut recovered).unwrap();
        }
        assert_eq!(recovered, plaintext);

        // Tampering any ciphertext byte fails Poly1305 authentication.
        let mut tampered = wire.clone();
        tampered[10] ^= 0x01;
        let mut r_bad = EncReader::new(&k_in).unwrap();
        assert!(r_bad.decrypt_into(&tampered, &mut Vec::new()).is_err());

        // Mismatched PSK produces a different key and fails authentication.
        let in_kp2 = EphemeralKey::generate().unwrap();
        let out_kp2 = EphemeralKey::generate().unwrap();
        let in_pk2 = in_kp2.public_bytes();
        let out_pk2 = out_kp2.public_bytes();
        let k_out2 = out_kp2
            .derive_feed_key(in_pk2, true, Some("key-A"))
            .unwrap();
        let k_in2 = in_kp2
            .derive_feed_key(out_pk2, false, Some("key-B"))
            .unwrap();
        assert_ne!(k_out2, k_in2);
        let mut wire2 = Vec::new();
        EncWriter::new(&mut wire2, &k_out2)
            .unwrap()
            .write_frame(b"hello")
            .unwrap();
        assert!(EncReader::new(&k_in2)
            .unwrap()
            .decrypt_into(&wire2, &mut Vec::new())
            .is_err());
    }
}
