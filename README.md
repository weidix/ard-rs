# ard-rs

Pure Rust, platform-independent parsing and framebuffer decoding for Apple
Remote Desktop (ARD) screen sharing.

## Native GPU viewer

`ard-viewer` is a receive-only desktop viewer for macOS, Windows, and Linux.
It connects directly to the ARD TCP service; it does not include a relay,
browser server, mouse input, or keyboard input.

```sh
cargo build --release --features viewer --bin ard-viewer
ARD_PASSWORD='screen-sharing-password' \
  cargo run --release --features viewer --bin ard-viewer -- \
  192.168.1.20:5900 username
```

The viewer defaults to Apple adaptive MVS (`1011`) with GPU tile/DCT decoding.
It also supports the native low, medium, and high profiles plus RDM-compatible
full quality (`[Zlib, ZRLE]`):

```sh
# Maximum fidelity and bandwidth
ard-viewer --quality full 192.168.1.20:5900 username

# Apple MVS adaptive streaming and GPU tile decoding (the default)
ard-viewer --quality adaptive 192.168.1.20:5900 username

# Optional minimum server update interval; zero is native maximum rate
ard-viewer --frame-interval-ms 16 192.168.1.20:5900 username
```

After encrypted transport activation, the client requests and decodes one
non-incremental full-frame baseline, then sends Apple's automatic frame-update
subscription with a zero interval by default. This preserves MVS copy/cache
state while letting `screensharingd` push later changes without waiting for a
decode/render cycle. The window title reports decoded framebuffer updates per
second and actual encrypted inbound Mbit/s.

In adaptive mode, the CPU performs MVS state and Rice/Huffman parsing and emits
bounded 8x8 tile commands with native DCT coefficients. A wgpu compute pipeline
performs inverse DCT, chroma expansion, tile composition, and color conversion
into a GPU-only presentation texture.

```text
TCP -> authenticated decrypt -> MVS tile/DCT parser
    -> GPU storage buffers -> compute IDCT/tile expansion
    -> GPU presentation texture -> Metal / D3D12 / Vulkan
```

The full, high, medium, and low profiles use the CPU RGBA decoder for Raw,
Zlib, ZRLE, and Apple's three sub-zlib encodings, then upload complete snapshots
to the same GPU presentation texture. Pending full-frame snapshots are
coalesced so a slow window cannot grow latency or memory without bound.

Reverse-engineering notes are in
[`docs/SCREENSHARING_RE.md`](docs/SCREENSHARING_RE.md). A step-by-step playbook
for reproducing the native-code investigation and oracle validation is in
[`docs/REVERSE_ENGINEERING_PLAYBOOK.md`](docs/REVERSE_ENGINEERING_PLAYBOOK.md).

ARD reuses RFB message framing, but it is not merely a normal VNC session. This
crate implements Apple-specific protocol behavior directly and does not depend
on a VNC library or a native operating-system library.

## Implemented

- Apple protocol banner `RFB 003.889`
- Apple security-type recognition (`30..=36`) without pretending those methods
  are ordinary VNC authentication
- bounded parsing of Apple type-30 Diffie-Hellman parameters and encrypted
  credential responses
- pure-Rust construction of the Apple type-30 client exchange from
  caller-supplied random input
- Apple `ClientInit` flag-byte and client message-10 session-options parsing
- bounded parsing of the 66-byte `RFBViewerInformation` (`0x21`) message
- byte-exact construction and bounded parsing of the `RFBSetEncryptionLevel`
  (`0x12`) proposal and its eight-byte activation record, matching the
  installed client and screensharingd handler
- explicit handling of the zero-sized encryption-control rectangle
  (`1103` / `0x044f`)
- incremental, transactional encrypted-record framing across arbitrary TCP
  fragmentation, with persistent AES-128-CBC state, implicit sequence
  validation, and SHA-1 verification before plaintext is returned
- a bounded incremental dispatcher that turns verified record payloads into
  server messages, routing FramebufferUpdate (including MVS `1011`) rectangles
  into the persistent decoder state and exposing `1103` controls
- a pure-Rust encrypted-transport oracle server that completes the type-30
  exchange, sends a real 1103 control rectangle, validates the client's
  activation and automatic-update subscription, and exchanges AES-CBC records
  carrying either MVS or persistent full-colour zlib frames
- bounded parsing of security offers, `ServerInit`, and `FramebufferUpdate`
- Apple's extended `ServerInit` command-support block, including the
  `0x12`-advertising bitfield that gates the encrypted transport
- client message generation for pixel format, encodings, and update requests
- Apple's server-driven automatic frame-update message (`0x09`), including a
  configurable interval and the native zero-interval maximum-rate default
- RDM-compatible low, medium, high, adaptive MVS, and full-quality encoding
  profiles
- raw and full-colour zlib rectangles
- ZRLE tiles (raw, solid, packed palette, plain RLE, and palette RLE)
- Apple encoding `1000`: zlib-compressed 1-bit halftone
- Apple encoding `1001`: zlib-compressed 4-bit grayscale
- Apple encoding `1002`: zlib-compressed RGB555 “thousands of colors”
- Apple encoding `1011` MVS framing, type-2 quantization-table updates, and
  partial-update solid, bilevel, repeat, left-copy, above-copy, and
  general Rice/DCT tile modes with zigzag, quantization, and inverse DCT,
  partial explicit/sequential cache records, plus full-update unchanged,
  copy-replay, differential DCT, JPEG chrominance Huffman, and
  explicit/sequential cache records
- independent persistent zlib state for every Apple stream
- RGBA framebuffer updates with checked dimensions, allocations, runs, and
  palette indexes
- optional receive-only native GUI with direct TCP authentication,
  server-driven encrypted session streaming, selectable quality, GPU-native
  MVS tile/DCT output, RGBA upload, live FPS/traffic metrics, and
  Metal/D3D12/Vulkan presentation

Apple MVS (`1011`) is identified as a distinct codec and is never fed to a VNC
or zlib decoder. Its two bitstreams, Rice/DCT state, per-tile differential
baseline, copy metadata, and 1–64999 DCT cache ring are decoded directly.

## Current end-to-end boundary

The `0x21`, `0x12`, `1103`, type-30 computation, encrypted-record, and
decrypted-payload dispatch layers are implemented and covered by focused
tests, including in-process client↔oracle sessions that decode both adaptive
MVS and full-quality persistent zlib frames from encrypted records. A Rust
client has also completed a private live
session against macOS `screensharingd` and decoded a fully covered framebuffer;
the captured payload and pixels are deliberately not stored in this repository.
See
[`docs/SCREENSHARING_RE.md`](docs/SCREENSHARING_RE.md) for confirmed evidence
and the exact remaining work.

## Pure Rust guarantee

Core runtime dependencies are Rust-only crates: `flate2` with its
`rust_backend`, RustCrypto AES/digest primitives, `num-bigint`, and `subtle`.
The optional `viewer` feature adds winit and wgpu, selecting the operating
system's normal windowing and GPU backend. Project code retains
`#![forbid(unsafe_code)]`.

## Verification

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --features viewer
cargo clippy --all-targets --features viewer -- -D warnings
cargo check --target aarch64-unknown-linux-musl
cargo check --target x86_64-unknown-linux-musl
cargo check --target x86_64-pc-windows-gnu
cargo check --target wasm32-wasip1
cargo check --target x86_64-unknown-linux-musl --features viewer --bin ard-viewer
cargo check --target x86_64-pc-windows-gnu --features viewer --bin ard-viewer
```

The test suite includes the exact ARD banner and security offer captured from
the local macOS Screen Sharing server, Apple type-30 authentication framing,
independent byte-level vectors for all three Apple zlib subencodings, a
persistent-stream test, a complete FramebufferUpdate, and ZRLE compact-pixel
decoding. A local isolated Rust server was
also connected to macOS Screen
Sharing 6.1 (760.4): the native client advertised encoding `1011` and displayed
the exact 15-byte MVS type-0/repeat packet as a stable 64x64 white framebuffer.
A second dual-bitstream packet rendered one gray type-4 YCbCr tile followed by
63 white tiles and also remained connected.
A third packet used the minimum type-5 Rice/DCT record (zero DC predictors and
an immediate AC end-of-block); Apple's decoder rendered the expected gray tile.
An additional nonzero-AC frame produced the same eight-value luminance ramp in
Apple's decoder and this Rust implementation.
The native decoder also accepted a stateful partial-Rice → full-differential
sequence containing a nonzero standard-JPEG chrominance Huffman coefficient,
then accepted both partial and full explicit cache recalls of the generated
DCT tile.
