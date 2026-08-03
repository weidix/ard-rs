# ARD offline fixtures

- `macos-ard-handshake.bin` is the 18-byte public, pre-authentication server
  greeting captured from the local macOS Screen Sharing service on 2026-08-03.
  It contains the `RFB 003.889` banner and security-type offer only.
- `native-mvs-white-64x64.bin` is a complete plaintext ARD
  `FramebufferUpdate` containing one MVS (`1011`) rectangle. The exact MVS
  packet was accepted by macOS Screen Sharing 6.1 (760.4) and displayed as a
  64x64 white framebuffer on 2026-07-25.
No fixture contains credentials, session keys, encrypted credential blocks,
clipboard data, or captured user pixels. Every fixture stores protocol bytes
directly as `.bin`; no duplicate hexadecimal form is maintained. Use
`xxd -g1 FILE.bin` when a readable byte dump is needed.

Replay them without a live Screen Sharing connection:

```sh
cargo test --test offline_capture
```

Decode the saved MVS frame into a viewable PPM image:

```sh
cargo run --example decode_offline_capture
```
