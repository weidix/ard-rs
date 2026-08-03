# macOS Screen Sharing decoder findings

The implementation was derived from the Screen Sharing components installed on
the test Mac, not by assuming that ARD is interchangeable with VNC.

## Installed implementation

- application:
  `/System/Applications/Utilities/Screen Sharing.app`
- application version: `6.1 (760.4)`
- XBS project marker: `RemoteDesktop-760.4`
- decoder image:
  `/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing`
- source paths retained in the image:
  - `RFBViewerLib/DecodeRaw.c`
  - `RFBViewerLib/DecodeZlib.c`
  - `RFBViewerLib/DecodeZRLE.c`
  - `RFBViewerLib/DecodeMultiVariant.c`
  - `RFBViewerLib/DCTHuffmanDecode.c`
  - `RFBViewerLib/ServerMessages.c`

The framework is stored in the macOS dyld shared cache. `dyld_info` exposes its
symbols, constants, strings, and disassembly without copying or linking it into
this crate.

The `0x12` transport findings below were re-verified on macOS 26.6 build
25G72 (Screen Sharing `6.1`) on 2026-08-03; unslid addresses in this document
apply to that build only.

## Confirmed encoding constants

The framework data symbols contain:

| Symbol | Value | Rust handling |
| --- | ---: | --- |
| `kSSVideoEncoding_SubZlibHalftone` | 1000 | 1 bit/pixel, MSB first |
| `kSSVideoEncoding_SubZlib16Gray` | 1001 | 4 bits/pixel, high nibble first |
| `kSSVideoEncoding_SubZlibThousandsCodec` | 1002 | big-endian RGB555 |
| `kSSVideoEncoding_MultiVariantScreenshare` | 1011 | distinct MVS codec |
| `kSSVideoEncoding_Zlib` | 6 | negotiated-pixel zlib |
| `kSSVideoEncoding_ZRLE` | 16 | ZRLE |

The installed quality lists are:

- low: `[1000, 6, 16]`
- medium: `[1001, 6, 16]`
- high/adaptive: `[1011, 1002, 6, 16]`
- full quality: `[6, 16]`

## Apple zlib rectangle framing

`DecodeZlibUpdate` first reads a four-byte big-endian compressed length. It
selects one of four persistent inflate states according to the encoding:

| Encoding | Exact decompressed bytes per row |
| ---: | ---: |
| 1000 | `ceil(width / 8)` |
| 1001 | `ceil(width / 2)` |
| 1002 | `width * 2` |
| 6 | `width * negotiated_bytes_per_pixel` |

The decoder calls inflate with sync-flush semantics. Reinitializing inflate for
every rectangle is therefore incorrect even when the first rectangle happens
to decode.

The grayscale tables in Apple's `DecodeGrayscale16/32` use `n * 0x10` for
levels 0 through 14 and promote level 15 to full white. The 32-bit thousands
path expands RGB555 into 8-bit RGB.

## ZRLE

The installed framework has separate functions for raw pixels, one color,
packed palettes, plain RLE, and palette RLE:

- `DecodeZRLERawPixels`
- `DecodeZRLEOneColor`
- `DecodeZRLEPalette`
- `DecodeZRLEPlainRLE`
- `DecodeZRLEPaletteRLE`

The Rust implementation mirrors those cases while enforcing tile, palette,
run-length, input, and output limits.

## Live observations

The local launchd-managed Screen Sharing service listened on TCP port 5900 and
returned:

```text
RFB 003.889\n
05 1e 21 24 02 23
```

That is five security types (`30, 33, 36, 2, 35`), four of which are in
Apple's IANA allocation. A standard Screen Sharing connection to another Mac on
the LAN was also established successfully, confirming that this is the active
decoder path rather than dead compatibility code.

### Authentication types offered by the tested Mac

The active macOS 26.6 server configuration offered exactly the following
authentication methods, in wire order:

| Type | Meaning | Current crate support |
| ---: | --- | --- |
| `30` | Apple Remote Desktop Diffie-Hellman username/password authentication | implemented and validated against the real server |
| `33` | Apple `RSA1` authentication negotiation; supports an RSA key request and RSA-protected plain-password or SRP submodes | recognized only |
| `36` | Apple Secure Remote Password (SRP) authentication | recognized only |
| `2` | standard RFB/VNC password authentication | recognized only |
| `35` | Apple Kerberos authentication | recognized only |

This list describes the methods advertised by this Mac under its current
configuration; it is not the complete Apple allocation. IANA assigns the whole
[`30` through `36` range to Apple](https://www.iana.org/assignments/rfb/rfb.xhtml),
but does not publish names for the individual values. Types `31`, `32`, and
`34` were not present in the captured offer.

The private subtype names above were confirmed from the installed
`screensharingd` authentication dispatcher. Type `33` enters
`SendRSAResponse` and accepts packets marked `RSA1`, with key-request,
plain-authentication, and SRP-authentication handlers. Type `35` enters
`HandleKerberosAuthenticationMessage`, while type `36` enters
`HandleSRPAuthenticationMessage` and `SendSRPChallenge`.

For Apple security type `30`, the server message is a two-byte generator, a
two-byte key length, then a prime modulus and server public key of that length.
The client reply is a 128-byte encrypted credential block followed by a client
public key of the negotiated length. The Rust parser handles and bounds both
messages as ARD-specific authentication records.

The current native client also extends initialization. It sent `0xc1` in the
one-byte `ClientInit` position (so it must be preserved as flags rather than
restricted to the standard `0/1` boolean), followed after `ServerInit` by
client message `10 00 00 01`. The Rust protocol layer parses both records.

## MVS wire structure

MVS (`1011`) is implemented by `DecodeMultiVariant.c` plus
`DCTHuffmanDecode.c`; it has its own bitstream, cache/copy modes, YCbCr
conversion, DCT, and Huffman state. `DecodeMVSUpdate` first reads a four-byte
big-endian update length and dispatches on the first payload byte:

| Type | Meaning confirmed from the installed decoder |
| ---: | --- |
| `0` | partial update; bytes 1–2 are Rice parameters, bytes 3–5 are a 24-bit buffer offset, bitstream starts at byte 6 |
| `1` | full DCT update; bytes 1–2 are tile limits (clamped to 64), bitstream starts at byte 3 |
| `2` | quantization update; exact payload length 129, followed by two 64-byte 8x8 tables |

The full-update tile selector is two bits. The partial-update selector is three
bits and includes repeat counts, framebuffer copies, solid/bilevel YCbCr,
Rice/DCT blocks, and cache operations. Both paths use state across rectangles.
Partial updates end both bitstreams with `0x6d`; full updates end with
`0x6d 0x76 0x73`.

For partial updates, the stream before the 24-bit offset contains the initial
state bit, three-bit selectors, repeat counts, and its marker. The stream at
the offset contains type-3 bitmaps, type-4 mode/color/bitmap data, cache
indices, Rice/DCT data, and its own marker. This split was confirmed in
`DecodeMVSUpdate` and by the native decoder oracle.

The Rust decoder currently parses and retains type-2 quantization updates. For
type 0 it implements repeat counts, white tiles, left/above copies, bilevel
tiles, new/reused solid or two-color YCbCr tiles, and type-5 Rice/DCT tiles.
The latter includes DC Rice prediction, both AC magnitude phases, short
positive/negative coefficients, EOB/run records, zigzag placement,
quantization, a pure-Rust 8x8 inverse DCT, and within-update block reuse.
Decoding is transactional: an invalid Rice or cache command does not leave a
partially modified framebuffer. Type-0 selectors 6 and 7 recall explicit and
sequential entries from the DCT cache. Type-1 implements selector 0
(unchanged), selector 1 (per-tile differential luma plus standard JPEG
chrominance AC Huffman), selector 2 (replay of a still-valid partial-copy
source), and selector 3 (explicit or sequential cache recall), followed by the
`0x6d 0x76 0x73` trailer. Newly decoded type-1 DCT tiles populate Screen
Sharing's 1–64999 cache ring. MVS is never routed through zlib or a generic VNC
decoder.

The direct-color YCbCr path uses Screen Sharing's exact lookup-table rounding:
red and blue use symmetrically rounded 16.16 coefficients `91881` and `116130`;
green combines coefficients `22554` and `46802` with a `32768` half-unit bias.
This differs by one color level from a naïve signed right shift for some
negative chroma values.

## Native decoder oracle

An isolated pure-Rust one-shot server in `examples/mvs_oracle_server.rs` was
cross-compiled for `aarch64-unknown-linux-musl` and run in a local Linux
container so macOS would not reject a self-connection. Screen Sharing 6.1
(760.4) completed the `RFB 003.889` handshake, sent `ClientInit` flags `0xc1`
and session options `10 00 00 01`, advertised 13 encodings including MVS
`1011`, and requested a framebuffer update.

The server then sent the same 15-byte MVS packet covered by
`decodes_native_screen_sharing_oracle_mvs_packet`: type 0, a white-tile command
with an extended repeat count of 63, and both `0x6d` stream markers. Apple's
installed decoder displayed a stable white 64x64 framebuffer. This validates
the MVS length framing, partial-update header, long repeat coding, tile count,
and stream markers against the real decoder rather than only this crate's own
implementation.

A second oracle packet, covered by
`decodes_native_screen_sharing_solid_oracle_packet`, put one type-4 selector
and a repeated type-0 selector in the primary stream, with a neutral YCbCr
color in the secondary stream. Screen Sharing rendered an 8x8 gray tile at the
upper-left and white for the other 63 tiles, then remained connected. This
validates the two streams' roles and the type-4 solid-color path.

A third oracle packet used type 5 with the minimum valid `ExpandBlockRice`
record: zero DC Rice data and an immediate AC end-of-block. Screen Sharing
rendered the expected neutral-gray 8x8 tile followed by 63 white tiles and kept
the connection open. The identical packet is covered by
`decodes_native_screen_sharing_zero_dct_oracle_packet`.

The nonzero-AC oracle used a positive base coefficient at zigzag index 1 and
reused that block for the complete frame. Apple's installed decoder produced
the repeating luminance row `159, 154, 145, 134, 122, 111, 102, 97`; the Rust
Rice decoder, quantization, and inverse DCT produce the exact same row.

A stateful oracle then established a type-5 Rice baseline, sent a type-1
differential tile with a nonzero standard-JPEG chrominance AC Huffman
coefficient, and recalled the resulting cache entry through both a type-0
explicit-cache selector and a type-1 explicit-cache selector. Screen Sharing
decoded all four updates and remained connected. The zero-limit differential
case also explains the native uniform value observed during reverse
engineering: the prior scan-one value becomes the new DC coefficient, yielding
exactly 160 in the Rust decoder.

## Modern encrypted transport: current status

This section records the current implementation boundary. It deliberately
separates confirmed protocol facts from incomplete end-to-end validation.

### Confirmed `RFBViewerInformation` message (`0x21`)

The installed framework retains the symbol `_RFBViewerInformation` at
`0x1c81d6134`. Its builder writes exactly 66 bytes:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 1 | client message type `0x21` |
| 1 | 1 | zero padding |
| 2 | 2 | big-endian payload length `62` |
| 4 | 2 | big-endian structure version `1` |
| 6 | 16 | four big-endian viewer components |
| 22 | 12 | macOS major, minor, and patch components |
| 34 | 32 | capability/reserved bytes |

The live macOS 26.5.2 capture contains viewer components `[2, 6, 1, 0]`.
The capability block is preserved as opaque data because individual bit
meanings are not yet proven.

The Rust parser now:

- enforces the message type, zero padding, fixed version, and fixed length;
- applies a caller-provided total-message limit before reading the payload;
- returns the exact number of consumed bytes;
- preserves all four viewer components and all 32 capability bytes;
- has normal, every-prefix truncation, over-limit, overlong, bad-padding, and
  unsupported-version tests.

### Confirmed `RFBSetEncryptionLevel` messages (`0x12`)

The installed Screen Sharing framework exports `_RFBSetEncryptionLevel`
(image-relative `0x000EE920`, unslid `0x1C892F920` on macOS 26.6 build
25G72). It validates the connection magic and an internal enabled flag, then
writes and sends a fixed 12-byte message for level 1:

```text
12 00 00 01 00 01 00 01 00 00 00 01
```

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 1 | client message type `0x12` |
| 1 | 1 | zero padding |
| 2 | 2 | big-endian command; `1` proposes encryption methods |
| 4 | 2 | big-endian encryption level; `0` or `1` |
| 6 | 2 | big-endian method count |
| 8 | 4 × count | big-endian 32-bit method identifiers |

The screensharingd `HandleSetEncryptionMessage` (ViewerMessages.c) reads an
8-byte header, byte-swaps the two big-endian 16-bit fields inside the header,
and for command 1 requires the method count to be at most 100, then accepts
the message only if one of the big-endian method identifiers equals `1`
(ComCryption). It records the level and clears its two 16-byte wrapped
session-block slots before sending the 1103 rectangle.

After the client accepts the 1103 control rectangle, `_HandleFramebufferUpdate`
in the framework builds and sends a fixed 8-byte message (command 2):

```text
12 00 00 02 00 01 00 00
```

The server's command-2 path requires the level field to be `1` and then
transitions to decrypting everything received (`**going to decrypt everything
that is received`); any other value stops encryption. This is the "later
eight-byte control message" from the previous unfinished-work list.

The Rust library now:

- builds both messages with the exact native byte layout
  (`build_ard_set_encryption_level` and `build_ard_encryption_activation`);
- bounds the level to `0..=1` and the method count to `0..=100`;
- parses command 1 with its method list and command 2 as a fixed eight-byte
  activation record with the same bounds as the native handler;
- rejects unknown commands, nonzero activation counts, bad padding, and every
  prefix truncation.

Evidence level: confirmed/native for the client builder and server handler on
macOS 26.6 build 25G72. The live exchange sequence (exact position of `0x12`
relative to `0x21`, `SetEncodings`, and the 1103 rectangle) still needs
confirmation from a new session driven by the Rust implementation.

### Confirmed extended `ServerInit`

The client only sends `0x12` after checking `RFBServerCommandSupported`,
which reads a 16-byte command-support bitfield stored in the connection
state. The bitfield is MSB-first per byte: command `c` lives in byte `c / 8`
at bit position `7 - (c % 8)`. `screensharingd`'s `SendServerInitialiation`
advertises this bitfield inside an Apple extension appended to the standard
ServerInit header:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 2 | width |
| 2 | 2 | height |
| 4 | 16 | pixel format |
| 20 | 4 | big-endian payload length (`22 + name_len`) |
| 24 | 2 | zero marker |
| 26 | 4 | big-endian flags word |
| 30 | 16 | command-support bitfield |
| 46 | name_len | machine name |

The client takes the extended path only when the payload length is at least
22 and the two-byte marker is zero; otherwise it falls back to a plain server
name and the default command set (which does not include `0x12`). This is why
the earlier MVS oracle, which sent a standard ServerInit, never received a
`0x12` proposal from Screen Sharing.

The Rust library now:

- parses the extension and exposes `flags`, the 16-byte `command_support`
  bitfield, and `supports_command(c)`;
- builds the extended ServerInit (`build_ard_server_init`) with a caller
  supplied bitfield;
- keeps parsing plain (non-extended) ServerInit messages unchanged.

Evidence level: confirmed/native for both the client parse path and the
screensharingd builder on macOS 26.6 build 25G72. Whether Apple's client
accepts a test-server bitfield that differs from screensharingd's exact value
still needs the native oracle run.

### Confirmed encryption control rectangle (`1103` / `0x044f`)

The native framebuffer-update path recognizes a zero-sized rectangle and then
reads exactly 36 payload bytes (`_ReadSocketData` at `0x1c81ddb34`).
The layout is:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 4 | big-endian command; the observed and accepted value is `1` |
| 4 | 16 | wrapped CBC session value |
| 20 | 16 | wrapped initial chaining value |

The command comparison is visible at `0x1c81ddba8`–`0x1c81ddbbc`.
The two blocks are separately transformed in place at `0x1c81deb44` and
`0x1c81dfa58`. The subsequent `CCCryptorCreate` call at `0x1c81e019c`
uses the first result as the AES-128 CBC value and the second as the initial
chaining value. They are therefore not two directional values.

Rust now treats encoding 1103 as a control rectangle before normal framebuffer
bounds and zero-size handling. It validates that all rectangle coordinates and
dimensions are zero, parses the fixed payload transactionally, and exposes the
control object separately from pixel rectangles. Its diagnostic formatting
always redacts both 16-byte blocks.

### Confirmed type-30 derivation

`_AuthDHClientGetModAndKey` computes the Diffie-Hellman shared integer, serializes
it to the negotiated modulus width, and applies MD5 at
`0x1c81dc874`–`0x1c81dc898`. The resulting 16-byte authentication-stage value
initializes the AES contexts used by the type-30 response and later unwraps the
two 1103 control blocks.

The crate now implements this path in pure Rust:

- bounded modulus and random-input lengths;
- public-parameter range validation;
- modular exponentiation and fixed-width big-endian serialization;
- MD5 derivation;
- fixed two-field type-30 response construction;
- AES-128 block transformation;
- redacted diagnostic formatting.

Random input and unused field bytes are supplied by the caller; the library
does not provide a weak fallback generator.

### Confirmed encrypted-record format

After the control rectangle, both live capture directions consist of records
framed as a big-endian `u16` length followed by that many ciphertext bytes.
Every observed length is nonzero and a multiple of 16. The server capture
contains 110,496 complete records and reaches EOF exactly when parsed this way.

The native `_EncryptOneMessage` and
`_DecryptOneMessageWithComCryption` routines establish this plaintext layout:

| Field | Size |
| --- | ---: |
| payload length | 2-byte big-endian value |
| payload | declared length |
| padding | through the final 20-byte boundary |
| checksum | 20-byte SHA-1 result |

The checksum input is the implicit 32-bit sequence number in big-endian order,
followed by every plaintext byte before the final checksum. Each direction has
its own sequence beginning at zero. CBC state persists across record
boundaries.

The Rust implementation now provides:

- incremental framing across arbitrary TCP fragmentation;
- multiple records per input;
- nonzero, block-aligned, per-record, and per-input-count limits;
- persistent CBC state with the 1103-provided initial chaining value;
- SHA-1 verification before the embedded length is trusted or plaintext is
  returned;
- implicit sequence tracking and exhaustion handling;
- rejection of modification and replay;
- transactional rollback for a failed record and for a multi-record input
  batch;
- a matching client-to-server record encoder;
- redacted state diagnostics.

The CBC helper is checked against the NIST SP 800-38A AES-128-CBC vector. The
targeted transport suite currently contains 13 passing tests, the type-30
suite contains 3 passing tests, the decoder suite contains 28 passing tests,
the library unit suite contains 7 passing tests, the new `0x12` suite contains
5 passing tests, the decrypted-payload dispatcher suite contains 6 passing
tests, the extended ServerInit suite contains 4 passing tests, and the
encrypted-session loop contains 1 passing test. A complete all-target run is
still required after the remaining work.

### Decrypted-payload dispatcher

`_WaitForEncryptedMessage` appends each verified record payload to the same
net buffer that ordinary server messages are read from, so the plaintext
stream is the normal RFB/ARD server-message stream. The Rust library now
provides a bounded incremental dispatcher (`ArdMessageDispatcher`) that:

- accepts arbitrary record-payload fragments and parses complete messages;
- routes FramebufferUpdate messages through the existing `Decoder`, including
  encoding-1011 MVS rectangles into the persistent MVS state machine;
- exposes zero-sized 1103 encryption-control rectangles as a distinct message;
- handles Bell and bounded UTF-8 ServerCutText;
- rejects unsupported message types instead of treating them as rectangles;
- enforces total buffered-message and cut-text limits and does not consume a
  malformed message from the buffered stream.

This dispatcher has now consumed the real decrypted plaintext stream described
below, including the native zero-sized quantization update and a real MVS
desktop update.

### Encrypted-transport oracle

`EncryptedTransportOracle` (library module `oracle`, CLI wrapper
`examples/encrypted_transport_oracle.rs`) is a one-shot pure-Rust server that
drives the whole modern path against a client:

1. type-30 challenge (RFC 3526 group 2, server exponent 1) and server-side
   `MD5(client_public_key)` derivation;
2. extended ServerInit advertising command `0x12`;
3. parsing of `0x21` and the `0x12` proposal;
4. a real 1103 control rectangle whose two blocks are AES-128-encrypted with
   the derived authentication value;
5. validation of the client's eight-byte activation message;
6. AES-CBC records carrying MVS white and solid rectangles, with the client
   direction decrypted and redacted (message types only).

The in-process integration test `tests/encrypted_session_loop.rs` runs a Rust
client through the complete session: banner, type-30 exchange, extended
ServerInit, `0x21`/`0x12`, 1103 unwrap, activation, encrypted
FramebufferUpdateRequests, and decoding of the white MVS frame from decrypted
records. This validates the full Rust stack end to end and prepares the exact
artifact needed for the native-client run: build for
`aarch64-unknown-linux-musl`, run it in a Linux container, and connect Screen
Sharing to it. That native run still has to happen; until it does, the
extended ServerInit bitfield and the wrapped-block layout remain
confirmed/native rather than confirmed/oracle.

### Real encrypted desktop capture

On 2026-08-03, `examples/capture_real_desktop.rs` was cross-compiled for
`aarch64-unknown-linux-musl` and run from an isolated Linux container against
the host's macOS 26.6 `screensharingd`. The Rust client completed type-30
authentication, sent the live `0x21` and `0x12` messages, unwrapped the real
1103 control, activated both encrypted directions, and requested the upper-left
256x256 framebuffer region.

The server returned two verified encrypted records whose payloads concatenate
to 4,448 bytes of ordinary ARD server-message data:

1. a 149-byte `FramebufferUpdate` carrying a zero-sized MVS type-2
   quantization-table rectangle;
2. a 4,299-byte `FramebufferUpdate` carrying a 272x272 MVS desktop rectangle.

The first record exposed a real decoder defect: zero-sized rectangles were
previously discarded before MVS type-2 control data could be consumed. The
decoder now routes zero-sized MVS rectangles through the codec while rejecting
zero-sized type-0/type-1 image updates. This partial capture was superseded by
the complete-frame fixture described below and is no longer stored separately.

The first decoder pass showed magenta/purple 8x8 blocks in otherwise neutral
UI regions. Comparing the type-5 chroma predictor with the installed
`ExpandBlockRice` implementation found an extra sign correction before signed
division. Rust already divides signed integers with truncation toward zero, so
the correction made negative even Cb/Cr predictors drift by `+2` on every new
block. Removing it restores neutral colors in the saved real frame; the hash
above covers the corrected output and a focused unit test covers positive,
negative, odd, even, and nonzero-delta predictor cases.

A second live request captured the complete framebuffer reported by the same
Mac. The server returned 53,215 plaintext bytes in three authenticated
encrypted records, containing two framebuffer updates. The decoder recovered
all 2,073,600 pixels of the 1920x1080 desktop, matching the current display's
native 1920x1080 mode. After all decoder fixes, the complete-frame PPM has
SHA-256 `cd8833a6d1c937cba2aa57fa47f7c5f88883eb9b3cfb44ad573d14d517d405c7`;
the saved plaintext stream has SHA-256
`5681ac38d2d73b56ae4c9fbf9b5c4b3b26bd47c2bd7fe8ffa8145aedc58044e7`.

Full-frame inspection also exposed inverted black/gray 8x8 regions in text.
Every affected tile was an MVS type-4 two-color tile. The native
`DecodeMVSUpdate` branch selects the second remembered color for a zero bit and
the first color for a one bit; the Rust call supplied those choices in the
opposite order. Correcting the order removes all detected anomalous dark 8x8
tiles, and a focused two-color bit-mask test preserves the native semantics.

The canonical live plaintext is stored only at
`tests/fixtures/real-macos-mvs-1920x1080.bin`. It can be replayed without a
network connection or password:

```sh
cargo run --example decode_plaintext_capture -- \
  tests/fixtures/real-macos-mvs-1920x1080.bin \
  1920 1080 \
  target/real-frame-full.ppm
```

Visual inspection of the complete replay confirms that both the magenta/purple
corruption and the inverted black/gray blocks are gone. Generated PPM/PNG files
remain under `target/`; only the single compressed plaintext input is committed
as a fixture.

The stored stream ends after the first fully covered type-0 MVS base frame. It
is therefore not a lossless reference image or a final-quality source-screen
capture: quantized luma softens fine detail, and Rice/DCT base tiles carry only
chroma DC for each 8x8 tile, which can visibly bleed color across sharp edges.
Apple's decoder applies the same codec structure. Decoder fidelity must be
compared against Apple's rendering of the same MVS bytes, not against the
uncompressed source desktop. Later type-1 differential updates, when present in
a longer stream, refine from the preceding DCT state and must be applied in
order.

The fixture contains compressed real desktop pixels, but no password,
authentication response, wrapped session block, session key, IV, encrypted
record, or clipboard data.

## Work not yet completed

The project now demonstrates real encrypted-desktop decoding, but is not yet a
complete modern Screen Sharing client.

1. **The native-client encrypted oracle run remains.** The Rust client is now
   live-confirmed against `screensharingd`, but Apple's client has not yet been
   run against `EncryptedTransportOracle` to provide the reverse-direction
   interoperability check.
2. **No native-reference comparison exists.** The required same-session native
   screenshot, dimensions, exact-pixel ratio, maximum channel error, and mean
   absolute error have not been produced.
3. **The committed structural vector is only partial.** The `0x21` fixture
   reproduces the live public structure. The 1103 tests currently use synthetic
   wrapped blocks; a minimal redacted real structural fixture is still needed.
4. **Final portability gates remain after live integration.** Full `fmt`,
   all-target tests, and linting pass. The four requested cross-platform target
   checks and a feature/dependency audit still need to be rerun after this live
   integration.

Until the remaining items are resolved, the implementation should be described
as a real-session-tested ARD decoder rather than a complete Screen Sharing
client.
