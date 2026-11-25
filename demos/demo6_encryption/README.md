# Demo 6: Hospital Wi-Fi Security (`enc/1` Wire Sniffer Comparison)

This demo places a transparent TCP packet sniffer between an LSL outlet and inlet to show the real bytes on the wire side by side:

- **Standard LSL (plaintext TCP):** clinical patient markers travel in cleartext and are immediately readable in the wire hex/ASCII dump.
- **`labstream enc/1` (X25519 + HKDF-SHA256 + ChaCha20-Poly1305):** the exact same markers travel as authenticated high-entropy ciphertext (`0` plaintext leaks on the wire) while the authorized inlet decrypts every marker intact.
- **Unauthorized probe test:** clicking **Test Unauthorized Client** sends a real plaintext `LSL:feed` request to the encrypted outlet and displays the live `LSL/110 403 Encryption required` rejection.

## Quick start

```bash
cd labstream
cargo run --release --manifest-path demos/demo6_encryption/Cargo.toml
```

Open `http://127.0.0.1:8785/` in a browser.
