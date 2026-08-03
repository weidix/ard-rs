# ARD offline fixtures

- `macos-ard-handshake.bin` is the 18-byte public, pre-authentication server
  greeting captured from the local macOS Screen Sharing service on 2026-08-03.
  It contains the `RFB 003.889` banner and security-type offer only.
- `native-mvs-white-64x64.bin` is a complete plaintext ARD
  `FramebufferUpdate` containing one MVS (`1011`) rectangle. The exact MVS
  packet was accepted by macOS Screen Sharing 6.1 (760.4) and displayed as a
  64x64 white framebuffer on 2026-07-25.
- `real-macos-mvs-1920x1080.bin` contains 53,215 decrypted plaintext bytes from
  a real macOS 26.6 `screensharingd` session captured on 2026-08-03. It contains
  a zero-sized MVS quantization-table update followed by the complete 1920x1080
  desktop update.

No fixture contains credentials, session keys, encrypted credential blocks,
or clipboard data. The real macOS fixture does contain compressed desktop
pixels and must therefore be treated as visual user data. Every fixture stores
the original protocol bytes directly as `.bin`; no duplicate hexadecimal form
is maintained. Use `xxd -g1 FILE.bin` when a readable byte dump is needed.

Replay them without a live Screen Sharing connection:

```sh
cargo test --test offline_capture
```

Decode the saved MVS frame into a viewable PPM image:

```sh
cargo run --example decode_offline_capture
```

Decode the complete real macOS fixture:

```sh
cargo run --example decode_plaintext_capture -- \
  tests/fixtures/real-macos-mvs-1920x1080.bin \
  1920 1080 \
  target/real-frame-full.ppm
```
