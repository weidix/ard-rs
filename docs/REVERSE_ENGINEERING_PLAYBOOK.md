# Apple Screen Sharing Reverse-Engineering Playbook

This document explains how to rediscover Apple Screen Sharing protocol
behavior, the MVS decoder (encoding `1011`), and the modern encrypted transport
on an installed macOS system, then reproduce the confirmed behavior in
testable, pure Rust.

It focuses on the investigation process. Confirmed protocol findings belong in
[`SCREENSHARING_RE.md`](./SCREENSHARING_RE.md).

## 1. Research boundary

The production implementation must remain:

- pure Rust;
- independent of ScreenSharing.framework, VideoToolbox,
  Security.framework, CommonCrypto, system zlib, and other native libraries;
- free of FFI and `extern "C"`;
- free of generic VNC client libraries that treat ARD as ordinary VNC;
- compiled with `#![forbid(unsafe_code)]`;
- bounded for network lengths, dimensions, record counts, allocations, and
  state transitions.

Installed macOS binaries and system tools may be used for read-only research
and validation. They must not become runtime dependencies.

## 2. Freeze the research environment

Apple can replace this implementation during a system update. Record the
environment before investigating:

```sh
sw_vers
uname -a
system_profiler SPSoftwareDataType
mdls -name kMDItemVersion \
  "/System/Applications/Utilities/Screen Sharing.app"
```

The original investigation used:

```text
Screen Sharing 6.1 (760.4)
XBS project marker: RemoteDesktop-760.4
```

Addresses and disassembly are valid only for the matching system build.
Relocate every function after an update instead of carrying old addresses
forward.

## 3. Locate the implementation

The primary targets are:

```text
/System/Applications/Utilities/Screen Sharing.app
/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing
/System/Library/CoreServices/RemoteManagement/screensharingd.bundle/Contents/MacOS/screensharingd
/System/Library/CoreServices/RemoteManagement/ARDAgent.app
```

Start with retained source paths, function names, and logs:

```sh
strings -a \
  /System/Library/CoreServices/RemoteManagement/screensharingd.bundle/Contents/MacOS/screensharingd \
  | rg -i "RFB|MVS|MultiVariant|Rice|DCT|Huffman|encrypt|decrypt|authentication"
```

ScreenSharing.framework resides in the dyld shared cache, but current Xcode
`dyld_info` can inspect it directly through the framework path:

```sh
xcrun dyld_info -segments \
  /System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing

xcrun dyld_info -exports \
  /System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing
```

Useful retained source paths include:

```text
RFBViewerLib/DecodeRaw.c
RFBViewerLib/DecodeZlib.c
RFBViewerLib/DecodeZRLE.c
RFBViewerLib/DecodeMultiVariant.c
RFBViewerLib/DCTHuffmanDecode.c
RFBViewerLib/ServerMessages.c
```

Source paths are navigation hints, not proof of a wire format.

## 4. Confirm encoding numbers from exported data

Find the exported symbols:

```sh
xcrun dyld_info -exports \
  /System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing \
  | rg "kSSVideoEncoding"
```

Then inspect `__TEXT,__const`:

```sh
xcrun dyld_info -section_bytes __TEXT __const \
  /System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing
```

The investigated build contains:

```text
_kSSVideoEncoding_SubZlibHalftone:          E8 03 00 00  -> 1000
_kSSVideoEncoding_SubZlib16Gray:            E9 03 00 00  -> 1001
_kSSVideoEncoding_SubZlibThousandsCodec:    EA 03 00 00  -> 1002
_kSSVideoEncoding_MultiVariantScreenshare:  F3 03 00 00  -> 1011
_kSSVideoEncoding_Zlib:                     06 00 00 00  -> 6
_kSSVideoEncoding_ZRLE:                     10 00 00 00  -> 16
_kSSVideoEncoding_AVCMediaStream:            F2 03 00 00  -> 1010
```

These values appear little-endian in the ARM64 image data. Do not identify
`1103` as AVC: the confirmed AVC constant is `1010`.

## 5. Produce searchable disassembly

The complete disassembly is large. Narrow it by symbol or retained string:

```sh
xcrun dyld_info -disassemble \
  /System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing \
  | rg -n -C 20 \
    "RFBViewerInformation|RFBSetEncryptionLevel|DecodeMVS|MultiVariant|Huffman"
```

Resolve an address to its containing symbol:

```sh
xcrun dyld_info -lookup_va 0xADDRESS \
  /System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing
```

Keep these address forms distinct:

- image-relative addresses printed by the export list;
- unslid virtual addresses in the dyld shared cache;
- runtime addresses after the ASLR slide.

Every documented address should include the macOS build and function name.
The function name is a better entry point for the next investigation than an
absolute address.

Maintain a worksheet for every target function:

| Item | Record |
| --- | --- |
| Entry point | Symbol, cross-reference, or unique log string |
| Inputs | Pointer, length, state object, rectangle |
| Bounds | Comparisons, failure branches, socket reads |
| State writes | Cache, predictor, CBC chaining value, sequence |
| Callees | Bit reader, IDCT, color conversion, cryptography |
| Result | Bytes consumed, error code, state transition |
| Evidence | Disassembly, capture, native oracle, or inference |

## 6. Recover the top-level MVS structure

Begin at `DecodeMVSUpdate`. Do not guess the compression algorithm first.
Establish:

1. the rectangle encoding is `1011`;
2. the first four payload bytes are a big-endian length;
3. the first byte inside that length is the update type;
4. each update type's fixed header size;
5. the start and terminal marker of every bitstream.

The confirmed structure is:

| Update type | Fixed structure |
| ---: | --- |
| `0` | Partial update; bytes 1–2 are Rice parameters and bytes 3–5 are a 24-bit secondary offset |
| `1` | Full differential/DCT update; bytes 1–2 are tile limits |
| `2` | 129-byte quantization update: type byte plus two 8×8 tables |

A partial update has two MSB-first bitstreams:

- primary: initial state, three-bit selector, repeat count, terminal marker;
- secondary: colors, bitmap, cache index, Rice/DCT data, terminal marker.

Both streams end in `0x6d`. A full update ends in:

```text
0x6d 0x76 0x73
```

Implement a bounded `BitReader` first, then add one selector at a time.
Translating the whole routine at once makes it difficult to distinguish a bit
order error from repeat, predictor, or IDCT errors.

## 7. Confirm selectors one at a time

Partial MVS uses three-bit selectors. Full MVS uses two-bit selectors. Use a
single-variable experiment for every branch:

1. encode one non-default tile;
2. encode all remaining tiles with a known white selector;
3. change only one bit field per experiment;
4. record whether Apple keeps the connection open;
5. capture the exact displayed pixels;
6. decode the identical bytes in Rust;
7. compare both results.

Confirmed partial branches include:

- white;
- copy from the left;
- copy from above;
- bilevel;
- solid/two-color YCbCr;
- Rice/DCT;
- explicit and sequential cache recall.

Confirmed full branches include:

- unchanged;
- differential DCT;
- copy replay;
- explicit and sequential cache recall.

## 8. Reproduce Rice, DCT, and Huffman incrementally

Implement and validate in this order:

1. zero DC with immediate EOB;
2. nonzero DC;
3. one nonzero AC coefficient;
4. block reuse;
5. multiple AC coefficients and runs;
6. differential baseline;
7. chrominance Huffman;
8. cache insertion and recall.

Keep an independent test vector for every step. One important nonzero-AC
regression vector produces this eight-pixel luminance row:

```text
159, 154, 145, 134, 122, 111, 102, 97
```

Apple's decoder and Rust must produce exactly the same values.

Confirm the standard JPEG chrominance AC table through both:

- native table data and lookup code;
- an Apple oracle packet containing a nonzero chrominance AC coefficient.

Do not adopt the standard table merely because the native data resembles
JPEG.

## 9. Reproduce color conversion and rounding

One-level YCbCr differences usually come from negative shifts and rounding.
Recover the exact integer constants from Apple's lookup tables or arithmetic.

Confirmed constants:

```text
Cr → R:  91881
Cb → B: 116130
Cb → G:  22554
Cr → G:  46802
```

Red and blue use symmetric rounding. Green combines its terms with a `32768`
half-unit bias. Tests must cover positive and negative chroma and clamping to
`0..=255`.

## 10. Build a native macOS decoder oracle

The repository contains:

```text
examples/mvs_oracle_server.rs
```

It is a minimal pure-Rust ARD server that accepts an authentication response
from an explicitly allowed peer and sends a handcrafted MVS rectangle.

macOS rejects some obvious self-connections. The original investigation put a
static Linux build in a local Docker container so Screen Sharing saw a
separate network endpoint.

Apple Silicon example:

```sh
rustup target add aarch64-unknown-linux-musl

cargo build --release \
  --example mvs_oracle_server \
  --target aarch64-unknown-linux-musl
```

Mount the static binary and publish the port:

```sh
docker run --rm -it \
  -p 5999:5999 \
  -v "$PWD/target/aarch64-unknown-linux-musl/release/examples/mvs_oracle_server:/mvs-oracle:ro" \
  alpine:3.22 \
  /mvs-oracle 5999 0.0.0.0 192.168.65.1 ard dct-ac
```

Docker Desktop's host-side source address may differ from `192.168.65.1`. If
the server reports `rejected non-local peer`, restart it with the exact peer IP
printed in that message. Do not remove the source restriction for convenience.

Connect Screen Sharing to:

```text
vnc://127.0.0.1:5999
```

The oracle accepts the local client's authentication response but does not
read or print plaintext credentials.

Available frame kinds are defined by the `match` in
`mvs_oracle_server.rs`, including:

```text
white
solid
dct
dct-ac
dct-full
dct-ac-full
full-diff
```

For every experiment, preserve:

- exact input bytes;
- rectangle dimensions;
- whether Apple disconnected;
- Apple's rendered result;
- Rust RGBA output;
- exact pixel differences.

## 11. Avoid self-confirming tests

Test vectors should come from independent sources:

1. real network captures;
2. the Apple native decoder oracle;
3. published standard vectors such as AES-CBC or JPEG Huffman vectors.

A test that uses the same helper as the implementation to calculate its
expected result often proves only that both copies make the same mistake.

MVS tests are in:

```text
tests/decoder.rs
```

Modern transport tests are in:

```text
tests/auth.rs
tests/encryption_transport.rs
```

## 12. Investigate a real connection

The transparent capture tool is:

```text
examples/capture_proxy.rs
```

Both authentication material and real desktop data are sensitive:

- never commit a complete capture;
- never print a password or derived key;
- redact keys, IVs, and wrapped blocks from `Debug`;
- state where temporary files were stored and how they were handled.

Locate the protocol layers in order:

```text
RFB banner
security offer and selection
authentication exchange
SecurityResult
ClientInit
ServerInit
SetEncodings
FramebufferUpdate
```

Do not scan for `00 00 03 f3` and assume every match is an MVS rectangle.
Compressed or encrypted data can produce false positives. Parse bounded
message boundaries from the outside inward.

## 13. Rediscover client message `0x21` and encoding `1103`

Useful native entry points in the investigated build:

```text
_RFBViewerInformation
_RFBSetEncryptionLevel
_EncryptOneMessage
_DecryptOneMessageWithComCryption
_AuthDHClientGetModAndKey
```

The native name of client message `0x21` is `RFBViewerInformation`. Confirm it
by:

1. locating `0x21 00 00 3e` in a real client stream;
2. finding the `_RFBViewerInformation` builder;
3. following every fixed-size write;
4. comparing changing fields across captures;
5. preserving the unknown 32-byte capability block as opaque bytes.

Rediscover encoding `1103` by:

1. parsing a zero-sized rectangle from a bounded FramebufferUpdate;
2. locating the `1103` comparison in native rectangle dispatch;
3. following the fixed `_ReadSocketData` length;
4. confirming the command comparison;
5. following both 16-byte block transformations and later
   `CCCryptorCreate` arguments;
6. validating the subsequent record framing against a real stream.

The confirmed `1103` payload is:

| Offset | Size | Content |
| ---: | ---: | --- |
| 0 | 4 | Big-endian command; the observed accepted value is `1` |
| 4 | 16 | Wrapped CBC session value |
| 20 | 16 | Wrapped initial chaining value |

Do not infer that the two 16-byte blocks are directional keys. Native data flow
shows that they become an AES-128 CBC value and its initial chaining value.

## 14. Validate the encrypted-record layer

Before attempting decryption, validate the structure after `1103`:

1. read a big-endian `u16`;
2. require a nonzero length;
3. require a multiple of 16;
4. skip that many ciphertext bytes;
5. confirm the parser reaches EOF without ambiguity;
6. record the count and length distribution.

Then recover the plaintext format from `_EncryptOneMessage` and
`_DecryptOneMessageWithComCryption`:

```text
u16 payload_length
payload
padding
20-byte SHA-1 checksum
```

The checksum input is:

```text
big-endian u32 sequence || all plaintext before the checksum
```

Each direction has a separate sequence beginning at zero. CBC chaining
persists across record boundaries.

An implementation must verify SHA-1 before trusting the embedded length or
returning plaintext. Authentication failure must roll back CBC state,
sequence, framing state, and output.

## 15. Understand the current real-capture limitation

The existing type-30 capture preserves the public exchange, `0x21`, `1103`,
and many encrypted records. It does not preserve the native client's one-time
internal random state, so the session keys cannot be reconstructed from the
public capture alone.

Consequences:

- the sample is useful for framing, ordering, length, and statistical checks;
- it cannot prove that the current Rust code recovers a real desktop;
- the next end-to-end run should let Rust generate and retain the type-30
  private random state;
- no capture offset, wrapped block, key, or expected frame may be hardcoded.

Consult the latest unfinished-work section in
[`SCREENSHARING_RE.md`](./SCREENSHARING_RE.md).

## 16. Translate native behavior into Rust

Translate one unit with explicit inputs and outputs at a time:

```text
wire parser
→ state transition
→ primitive
→ codec block
→ framebuffer integration
```

Implementation rules:

- state byte order explicitly for every wire integer;
- return exact consumed lengths from parsers;
- distinguish truncation from invalid input;
- enforce limits before allocating;
- stage or clone state and commit only on success;
- redact keys, IVs, passwords, and wrapped blocks;
- make arbitrary TCP fragmentation semantically invisible;
- do not hide a native fallback behind platform conditional compilation.

## 17. Assign evidence levels

Mark every documented conclusion with one of these levels:

| Level | Meaning |
| --- | --- |
| Confirmed/native | Direct native data flow or unambiguous disassembly |
| Confirmed/oracle | Apple and Rust agree on the same input |
| Confirmed/sample | Boundaries and fields agree across real samples |
| Inferred | Evidence supports it, but no independent validation exists |
| Unknown | Only position or length is known |

A function name, similarity to a standard algorithm, or one capture is not
complete proof.

## 18. Close a research iteration

Run:

```sh
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings

cargo check --target aarch64-unknown-linux-musl
cargo check --target x86_64-unknown-linux-musl
cargo check --target x86_64-pc-windows-gnu
cargo check --target wasm32-wasip1

cargo tree
```

Audit for forbidden native paths:

```sh
rg -n 'unsafe|extern "C"|#\\[link|libc|Security\\.framework|VideoToolbox|CommonCrypto' \
  src tests examples Cargo.toml
```

Claim real-desktop decoding only after completing this loop:

```text
real macOS server
→ Rust authentication and negotiation
→ Rust 1103 record decryption
→ Rust internal ARD message recovery
→ Rust image-encoding decode
→ RGBA framebuffer
→ PNG
→ comparison with a native screenshot from the same session
```

An MVS oracle, synthetic tests, transport unit tests, or successfully framed
records do not replace this end-to-end validation.

## 19. Shortest path for the next investigator

1. Freeze the macOS and Screen Sharing versions.
2. Run the full test suite to establish a baseline.
3. Read the confirmed and unfinished sections in `SCREENSHARING_RE.md`.
4. Relocate native symbols with `dyld_info` and check whether old addresses
   remain valid.
5. Replay the `white`, `solid`, `dct-ac`, and `full-diff` oracle cases.
6. Confirm exact Apple/Rust pixel agreement.
7. Check `0x21`, `1103`, and record framing in the real capture.
8. Establish a new session using type-30 random state controlled by Rust.
9. Complete `RFBSetEncryptionLevel` and bidirectional activation.
10. Feed verified plaintext to an incremental ARD dispatcher.
11. Export a real desktop PNG and compare it with the native view.
12. Update the protocol notes with new evidence and remaining unknowns.
