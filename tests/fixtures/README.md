# ARD offline fixtures

- `macos-ard-handshake.hex` is the 18-byte public, pre-authentication server
  greeting captured from the local macOS Screen Sharing service on 2026-08-03.
  It contains the `RFB 003.889` banner and security-type offer only.
- `native-mvs-white-64x64.hex` is a complete plaintext ARD
  `FramebufferUpdate` containing one MVS (`1011`) rectangle. The exact MVS
  packet was accepted by macOS Screen Sharing 6.1 (760.4) and displayed as a
  64x64 white framebuffer on 2026-07-25.
- `real-macos-mvs-256x256.hex` contains 4,448 decrypted plaintext bytes from a
  real macOS 26.6 `screensharingd` session captured on 2026-08-03. It contains
  a zero-sized MVS quantization-table update followed by a real MVS desktop
  update. The server framebuffer was 1920x1080; the saved visual regression
  checks the upper-left 256x256 pixels.

No fixture contains credentials, session keys, encrypted credential blocks,
or clipboard data. The real macOS fixture does contain compressed desktop
pixels and must therefore be treated as visual user data. Fixtures use
whitespace-separated hexadecimal so their bytes remain reviewable in source
control.

Replay them without a live Screen Sharing connection:

```sh
cargo test --test offline_capture
```

Decode the saved MVS frame into a viewable PPM image:

```sh
cargo run --example decode_offline_capture
```

Decode the real macOS fixture:

```sh
cargo run --example decode_real_capture
```
