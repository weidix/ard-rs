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

The installed Apple quality lists are:

- low: `[1000, 6, 16]`
- medium: `[1001, 6, 16]`
- high/adaptive: `[1011, 1002, 6, 16]`
- full quality: `[6, 16]`

Remote Desktop Manager exposes high and adaptive separately: high prefers the
16-bit sub-zlib codec (`1002`), while adaptive prefers MVS (`1011`). The viewer
therefore exposes these profiles (and appends DesktopSize `-223`):

| Viewer profile | Advertised image encodings |
| --- | --- |
| low | `[1000, 6, 16]` |
| medium | `[1001, 6, 16]` |
| high | `[1002, 6, 16]` |
| adaptive | `[1011, 1002, 6, 16]` |
| full | `[6, 16]` |

RDM documents full quality as full-colour zlib and adaptive quality as Apple
MVS in its [ARD session settings](https://docs.devolutions.net/fr/rdm/kb/knowledge-base/entry-settings/configure-ard-session-entry-in-rdm/).

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

## Confirmed automatic framebuffer updates (`0x09`)

The installed framework's `_RFBAutoFrameUpdate` builder sends a fixed 16-byte
client message. Multi-byte fields are big-endian:

| Offset | Size | Meaning |
| ---: | ---: | --- |
| 0 | 1 | message type `9` |
| 1 | 1 | zero padding |
| 2 | 2 | enabled flag (`1`) |
| 4 | 4 | minimum update interval in milliseconds |
| 8 | 2 | x |
| 10 | 2 | y |
| 12 | 2 | width |
| 14 | 2 | height |

The native client calls `_RFBAutoFrameUpdate(session, 1, interval, rect)` and
its default `frameUpdateInterval` is zero. The client converts seconds to
milliseconds, so the default wire interval is also zero and permits the
server's maximum supported update rate. `screensharingd` dispatches type `9`
to `HandleAutoFrameBufferUpdateMessage`, which starts monitoring screen
changes and pushes framebuffer updates.

Automatic updates do not replace initial framebuffer state establishment.
`-[SSSessionView requestUpdates]` invokes `-[SSSession stRequestUpdates]` when
display starts and retries up to five times on failure. The latter calls
`RFBCheckForUpdateCore(session, 1, NULL)`, which emits a type-`3` request with
`incremental=0`. The type-`9` interval method is a separate operation.

The previous Rust viewer sent incremental type-`3` requests only after
receiving and decoding the preceding update, serializing network latency and
decode time. Replacing that loop with type `9` accidentally also removed the
encrypted non-incremental startup request, so an MVS stream could begin with
copy/cache references before the decoder had a baseline. `ArdClient` now
requests and decodes one encrypted full baseline, then enables type `9` for
subsequent server-driven updates. Request-response polling remains available
as an explicit compatibility mode.

## Confirmed pointer-button masks

Apple's client uses Cocoa button order instead of the conventional RFB
middle/right order. On macOS 26.6 build 25G72, the exported constants are
`kSSLeftButton = 0`, `kSSRightButton = 1`, and `kSSOtherButton = 2`.
`-[SSEventSession stSendMouseButtonEvent:]` constructs the wire mask as
`1 << button` at `0x1c890a66c`-`0x1c890a67c`, then passes it to
`RFBPostMouseEventWithClickCount`. Its standard-message path stores that mask
unchanged as byte 1 at `0x1c892dc68` and writes the six-byte type-`5` message
at `0x1c892dc8c`. The resulting masks are therefore:

| Cocoa button | ARD pointer mask |
| --- | ---: |
| left | `0x01` |
| right | `0x02` |
| middle/other | `0x04` |

`-[SSFrameBufferView rightMouseDown:]` and `rightMouseUp:` load
`kSSRightButton` before creating the button event, confirming that a native
right click takes this path. Treating ARD as conventional RFB here swaps right
and middle clicks on the remote Mac.

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

## Confirmed inverse DCT and cache details

`_PerformInverseDCT8By8` is a wrapper around libjpeg's `jpeg_idct_islow`
(`_jpeg_idct_islow` at image-relative `0x1C8953F00` on macOS 26.6 build
25G72) plus `_ycc_xrgb_convert32to32`. The transform is two-pass with
explicit 32-bit wrapping arithmetic:

- the column pass descales with `(sum + 1024) >> 11`;
- the row pass descales with `(sum + 131072) >> 18` and clamps through the
  range-limit table;
- all-zero columns and rows take the exact DC shortcut (`dc << 2` in the
  workspace, then `(dc + 16) >> 5` plus 128 for the row).

The constants are the standard libjpeg `FIX_*` values (e.g.
`FIX_1_175875602 = 9633`, `FIX_2_053119869 = 16819`). A single-pass exact
two-dimensional sum rounds at a different precision and differs by one
level on roughly 0.3% of AC-rich samples, which shows up as faint speckle
at font edges and in gradients. The Rust decoder and both WGSL shaders now
implement the identical two-pass algorithm.

The type-1 full-update differential path stores refined tiles in a
1–64999 ring cache whose slot is only 0x63 bytes: all 64 luminance
coefficients as signed bytes, then the first 15 Cr and first 20 Cb
zigzag coefficients as signed bytes, and zeroes beyond that. A later cache
recall therefore renders smoother chroma than the tile's first render.
When the previous luma block is empty (`lumaCount == 0`), the differential
coefficient expansion still consumes `newCount` records (positions
1 through `newCount`), one more than the stored luma coefficients.

### ExpandBlockRice AC phase shifts

`ExpandBlockRice` descales AC coefficients by the log2 of a suffix width
that depends on the scan position and the limit: with a limit above 14 the
width is 2 below the limit and 8 at/above it (shifts 1 and 3); otherwise the
width is 8 below the limit and 16 at/above it (shifts 3 and 4). The shift is
selected at `0x1C89538C4`–`0x1C89538D8` (`csinc w17, w14, wzr, ge`;
`cinc w1, w14, lt`; `csel w17, w17, w1, hi` with `w14 = 3`), and the suffix
width itself is derived at `0x1C89535BC`–`0x1C89535CC`. A decoder that
always shifts by 4 at/above the limit doubles coefficients whenever the
limit exceeds 14, adding noise to Rice/DCT tiles. Verified pixel-identical
against the native decoder on a real captured session (limits 15/25).

Note: `dyld_info` misdisassembles `0x1C89538CC` (`0x1A8EB5C1`) as
`cinc w1, w14, ge`; the instruction is `cinc w1, w14, lt` (verified against
the assembler encoding). Rely on the empirically confirmed suffix-width
rule rather than the printed condition.

### Cursor rectangles inside FramebufferUpdate

Screen Sharing's client rectangle dispatch treats two additional encodings as
ordinary FramebufferUpdate rectangles:

| Encoding | Meaning | Payload |
| ---: | --- | --- |
| `1100` | pointer hotspot | zero (position is in the rectangle header) |
| `-239` | cursor image | variable cursor bitmap, read directly by the handler |

The server records `-239` and `1100` in `HandleSetEncodingsMessage` (flags at
viewer offsets `0x9a` and `0x9b` in the tested build) and only emits cursor
rectangles to viewers that advertise them. The native client still tolerates
an unadvertised `1100` rectangle; the Rust decoder now consumes it as a
zero-payload no-op instead of rejecting the whole FramebufferUpdate (which
dropped every frame while the pointer moved and forced a reconnect).

## Native decoder oracle

An isolated pure-Rust one-shot server in `crates/ard-core/examples/mvs_oracle_server.rs` was
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
transport, type-30, decoder, dispatcher, ServerInit, automatic-update, and
encrypted-session suites pass in both the core and feature-enabled all-target
runs, including strict Clippy linting.

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
`crates/ard-core/examples/encrypted_transport_oracle.rs`) is a one-shot pure-Rust server that
drives the whole modern path against a client:

1. type-30 challenge (RFC 3526 group 2, server exponent 1) and server-side
   `MD5(client_public_key)` derivation;
2. extended ServerInit advertising command `0x12`;
3. parsing of `0x21` and the `0x12` proposal;
4. a real 1103 control rectangle whose two blocks are AES-128-encrypted with
   the derived authentication value;
5. validation of the client's eight-byte activation message;
6. validation of the encrypted non-incremental type-`3` baseline request
   followed by the type-`9` automatic-update subscription;
7. AES-CBC records carrying either MVS white/solid rectangles or two updates
   from one persistent full-colour zlib stream, with the client direction
   decrypted and redacted.

The in-process integration tests run a Rust client through the complete
session: banner, type-30 exchange, extended ServerInit, `0x21`/`0x12`, 1103
unwrap, activation, encrypted full-baseline request, automatic-update
subscription, and decoding of both adaptive MVS and full-quality zlib frames
from decrypted records. They also verify that traffic accounting uses the
encrypted record bytes rather than the smaller decrypted message size. This
validates the full Rust stack end to end and prepares the exact
artifact needed for the native-client run: build for
`aarch64-unknown-linux-musl`, run it in a Linux container, and connect Screen
Sharing to it. That native run still has to happen; until it does, the
extended ServerInit bitfield and the wrapped-block layout remain
confirmed/native rather than confirmed/oracle.

### Private live validation and public synthetic fixture

The Rust client completed a private live encrypted session against macOS
`screensharingd`, including type-30 authentication, `0x21`/`0x12`, the 1103
control rectangle, encrypted records, and a complete framebuffer update. That
validation exposed zero-sized MVS control routing, signed chroma prediction,
and type-4 two-color selection defects. Each defect is retained as a focused
synthetic regression test.

No live payload or captured desktop pixels are stored in this repository.

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

## AVC media stream (encoding 1010) findings

Investigated on macOS 26.6 build 25G72 (Screen Sharing 6.1 / 760.4) on
2026-08-08. The third video mode, `kSSVideoEncoding_AVCMediaStream = 1010`,
is a dedicated real-time media path and is **not** a FramebufferUpdate
rectangle encoding: it has its own RFB messages, UDP/RTP transport, SRTP
encryption and H.264/HEVC payloads.

### Negotiation entry (confirmed)

The stock Screen Sharing profiles do **not** advertise `1010` in
`SetEncodings`. The high-performance profile places `1010` first so the
server selects the real-time path, then sends a dedicated RFB client message:

| Wire offset | Size | Field |
| ---: | ---: | --- |
| `0x00` | 1 | message type `0x1c` (28) |
| `0x02` | 2 | BE `messageSize` = total - 4 |
| `0x04` | 2 | BE `messageVersion` = `0x0300` |
| `0x06` | 4 | BE flags (bit0 video1 60fps, bit1 video2 60fps, bit2 send cursor, bit3 viewer app) |
| `0x0a`/`0x0c`/`0x0e` | 2 each | BE lengths of the three key blobs (46/46/46) |
| `0x14` | 16 | session UUID |
| `0x24` | 46 | audio key server→viewer |
| `0x52` | 46 | video1 offer |
| `0x80` | 46 | audio key viewer→server |
| `0xae` | 46 | video1 key server→viewer |
| `0xdc` | 46 | video2 key viewer→server |
| `0x10a` | 46 | video1 key viewer→server |
| `0x138` | 46 | video2 offer |
| `0x166` | 46 | audio offer |
| `0x194` | 46 | video2 key server→viewer |

Total with video2: 450 bytes; without video2 the builder emits only the first
six slots (312 bytes) and gates the audio offer behind video2. Recovered from
`_RFBMediaStreamServerConfiguration` (ScreenSharing.framework) and validated
against `HandleServerMediaStreamConfiguration` in screensharingd, whose
expected size formula is `216 + [0xa] + [0xc] + ([0xe] ? [0xe] + 0x5c : 0)`.

### Corrected viewer offer layout (2026-08-08, disassembly-confirmed)

The earlier 46-byte-slot reading above was wrong: 46 bytes is the **SRTP key**
length; the offers are **variable-length binary plists**. The exact layout was
recovered from `_RFBMediaStreamServerConfiguration` in ScreenSharing.framework
(macOS 26.6 build 25G72) and its caller
`-[SSSession stConfigureServerMediaStream]`:

| Offset | Size | Field |
| ---: | ---: | --- |
| `0x00` | 1 | RFB client message type `0x1c` |
| `0x02` | 2 | BE `messageSize` = total - 4 |
| `0x04` | 2 | BE version `0x0300` |
| `0x06` | 4 | BE flags (bit0 video1 60fps, bit1 video2 60fps, bit2 send cursor, bit3 viewer app) |
| `0x0a` | 2 | BE audio offer length |
| `0x0c` | 2 | BE video1 offer length |
| `0x0e` | 2 | BE video2 offer length (0 when absent) |
| `0x10` | 4 | reserved, zero |
| `0x14` | 16 | session UUID |
| `0x24` | 46 | audio key viewer→server |
| `0x52` | 46 | audio key server→viewer |
| `0x80` | n | audio offer (binary plist) |
| +n | 46 | video1 key viewer→server |
| +0x2e | 46 | video1 key server→viewer |
| +0x5c | m | video1 offer (binary plist) |
| +m | 46+46 | video2 keys (viewer→server, server→viewer) |
| +0x5c | k | video2 offer (only when present) |

Total without video2 = `0x80 + audio_len + 0x5c + video1_len`; with video2
add `0x5c + video2_len`. The `messageSize` field equals that total minus 4
(the native builder computes `0xd8 + audio + video1 + video2_extra`).
The real negotiator offers observed on the wire are 433–550 bytes (audio/video1
respectively), which is why the fixed-slot reading could not be right.

The offer plist is produced by `AVCMediaStreamNegotiator -createOffer`
(AVConference.framework). The dictionary contains at least:
`CallID` (a UUID string), `RemoteEndpointInfo` (a serialized `VCCallInfoBlob`
protobuf: `setCallID`, `setClientVersion`, `setDeviceType`, `setOsVersion`,
`setFrameworkVersion`, `setAudioDeviceUID`, `setDeviceName`), plus three keys
whose CFString constants sit at `__AUTH_CONST,__cfstring`
`0x1f3529620` (33 chars), `0x1f3529640` (28 chars) and `0x1f3529660` (33 chars)
in AVConference. Their values are an object from the negotiator settings ivar
`0x30`, an integer from ivar `0x48`, and the media-stream direction integer.
The exact constant strings are inside the dyld shared cache's coalesced
`__cstring` region and were not resolved statically; the Rust offer builder
uses best-effort names and must be validated against a live server.

The offers are **binary plists** (`NSPropertyListBinaryFormat_v1_0`, format
`0xc8`), produced by `AVCMediaStreamNegotiator -createOffer` with keys such as
`CallID` and `RemoteEndpointInfo`. Their lengths are variable and are carried
by the two-byte fields above.

### Server replies

The server answers over the TCP RFB session with dedicated messages built by
`EncodeRFBMediaStreamMessage1` / `EncodeRFBMediaStreamAnswer` /
`EncodeRFBMediaStreamErrorMessage` in screensharingd:

* `Message1` (RFB server message type `0x23`, 35): 68-byte struct with the
  AVC encoding marker `1010` at `0x1a`, UDP base port at `0x28`, base+1 at
  `0x2e`, base+2 at `0x34` (audio), and HDR flags at `0x30`/`0x36`
  (`0x02000000` = HDR). Message type confirmed from the viewer's
  `_RFBViewerLibCallback` jump table (`0x23` → `handleAVCMediaEncoding:`).
* `Message2` (answer): answer flags at `0x24`, three BE u16s at
  `0x28`/`0x2a`/`0x2c`, then the negotiator answer body.
* Error message: error type/subcode in the `0x1e` word.

The exact TCP record envelope around these replies (the server wraps them via
`SendOneBufferToViewer` with an 8-byte zero header, u32 length, u16 zero and
u16 BE length fields) still needs one live-capture pass; the parsers accept
both the raw 68-byte struct and the 16-byte-wrapped form.

### UDP transport (confirmed)

After `Message1`, the client binds one UDP socket per stream and talks to
consecutive server ports: audio = base, video1 = base+1, video2 = base+2.
Packets are RFC 3550 RTP. Payload encryption is SRTP AES-128-CM (RFC 3711):
each 46-byte negotiated key is 16-byte master key + 14-byte master salt; the
remaining 16 bytes are unused by AES-128-CM (reserved for the integrity layer).
Confirmed from `SSUDPSender`/`AVCVideoStream` in ScreensharingAgent and the
`AuthGetRandomBytes(0x2e)` key generation in the viewer.

When the remote Mac is behind an explicit UDP port forward, the external
destination can differ from the port carried by `Message1`. The viewer keeps
the advertised value as its local bind port and applies the configured
external value only to the remote socket address. Replacing both values would
break Screen Sharing's symmetric local-port assumption and can prevent the
server's first RTP datagram from reaching the viewer. Empty overrides preserve
the negotiated endpoint exactly.

### Codec and frame format (confirmed)

AVConference negotiates `rtpmap:123 H264/90000`, `rtpmap:126 X-H264/90000`
and `rtpmap:100 HEVC/90000`, with `fmtp` carrying `profile-level-id` and
`packetization-mode`. The RTP payloads are standard H.264 (RFC 6184 FU-A /
STAP-A) and HEVC (RFC 7798 FU / AP) access units; the server encodes with
VideoToolbox (`VTCompressionSessionCreate`, HEVC Main444 allowed) and pushes
IOSurface-backed frames through `AVCVideoStream`. The viewer decodes with
VideoToolbox in the same framework.

### Control plane (confirmed)

Mouse, keyboard, scroll and clipboard keep flowing over the TCP RFB session
as ordinary RFB messages; the UDP media path carries only the encoded audio
and video. `SSFrameBufferAVConferenceView` sends pointer events through the
RFB connection.

### Rust implementation

`crates/ard-core/src/media_stream/` implements the negotiation wire format, the
binary-plist negotiation payload parser, RTP de-packetization, SRTP
AES-128-CM and the UDP receive path in pure Rust (no unsafe, bounded parsing).
`crates/ard-viewer/src/media/vt.rs` provides the macOS VideoToolbox decode
backend and preserves the decoder's bi-planar YUV output through the GPU
display path.

### Fixed display modes and native NV12 rendering (2026-08-12)

Re-verified on macOS 26.6.1 build 25G76. Disassembly of
`_GetScreenSizes` in ScreenSharing.framework shows a fixed five-entry mode
table. Its logical sizes and 2x physical backing sizes are:

| Physical | Logical | Scale |
| --- | --- | ---: |
| 3840x2160 | 1920x1080 | 2.0 |
| 2880x1800 | 1440x900 | 2.0 |
| 3840x2160 | 1920x1080 | 2.0 |
| 2880x1620 | 1440x810 | 2.0 |
| 2624x1696 | 1312x848 | 2.0 |

The duplicate 1920x1080 entry is present in the native table. The
`stConfigure` caller supplies a mode count of five. A fixed configuration has
dynamic-resolution flags cleared, a 296-byte display record, and all five
modes; the selected logical mode replaces entry zero. Before serializing,
`_RFBSetDisplayConfiguration` takes the component-wise maximum physical size
across the entire table and multiplies each dimension by the exact binary64
constant with bits `0x3fb8a15b8a15b8a1`, narrowing the results to the two
binary32 physical-extent fields. The Rust builder mirrors that calculation and
rejects dimensions outside the four unique logical sizes instead of sending
integer values that the host silently ignores.

VideoToolbox output is requested as `420v` (NV12 video range). Decoder
callbacks also recognize `420f` and read the CoreVideo YCbCr matrix attachment
(BT.601, BT.709, or BT.2020). Each CVPixelBuffer plane is copied while held by
an RAII read lock, preserving native luma and interleaved chroma bytes. The
renderer uploads those bytes directly to persistent `R8Unorm` and `Rg8Unorm`
textures and performs range expansion, matrix conversion, and transfer
conversion in WGSL. The live path does not allocate or upload an RGBA desktop;
the example probe's CPU RGBA conversion exists only for diagnostic PNG output.

The server's four video SSRCs form slices of one reference chain. The receive
path now discards an incomplete timestamp when a newer completed access unit
arrives, resets depacketization after a sequence gap, and refuses predictive
frames until an H.264 IDR or HEVC IRAP frame recreates a clean VCP session.
Standard protected RTCP PSFB PLI is sent for the four media SSRCs after loss;
AVConference's installed Objective-C metadata exposes PLI, FIR, NACK, and
keyframe-request support. The four SSRCs are grouped by their shared RTP
timestamp, which is the actual sampling-instant boundary for one video frame;
each timestamp batch is submitted to VideoToolbox and published once. The
latest-frame mailbox merges distinct dirty slices so UI coalescing cannot lose
a region.

### Remaining work

* One live session capture to confirm the exact server-reply TCP envelope and
  the real offer plist byte layout (milestone ④).
* Windows MFT decode backend.
* SRTP authentication (HMAC-SHA1) if the negotiated suite requires it.

## Live capture follow-up (2026-08-08)

A real capture run was completed against Screen Sharing 6.1 on macOS 26.6
build 25G72 using the AVC oracle in
`crates/ard-core/examples/avc_oracle_server.rs` (cross-compiled to musl and
run in a Docker container on the `v6net` bridge so macOS accepts the
connection). Two workflow discoveries made the capture practical:

* **URL credentials bypass the login sheet.** `open "vnc://wei:x@IP:5901"`
  lets Screen Sharing complete the ARD DH exchange without UI interaction;
  the oracle accepts any credentials.
* **The client's DH response is 128 bytes of encrypted credentials plus a
  129-byte public key.** Reading 128+128 leaves the ClientInit byte
  misaligned; 128+129 keeps the stream in sync (ClientInit = `0xc1`).

### Confirmed client stream (plain ServerInit)

After `SecurityResult` the native client sends:

```text
c1                                   ClientInit (shared)
0a 00 00 01                          session options
02 00 00 0d [13 encodings]           SetEncodings
00 [pixel format]                    SetPixelFormat
02 00 00 0d [13 encodings]           SetEncodings (again)
03 [incremental=0, 1920x1080]        FramebufferUpdateRequest
09 ...                               16-byte ARD update-request variant
```

The 13 encodings are `[1011, 1002, 6, 16, -239, 1104, 1100, -223, 1101,
1105, 1107, 1109, 1110]` — **1010 (AVC) is never advertised**, and the client
never sends the `0x1c` media-stream message on a plain RFB session, even when
the server advertises every command bit in the extended ServerInit.

### Why the stock client never negotiates AVC

`screensharingd`'s `SendFrameBuffer` calls `InitializeUDPVideoStream`
(which emits the media-stream Message1) and then checks
`cmpl $0x3f2, 0x8c(%viewer)` — the AVC path requires the viewer's
`preferredCodec == 1010`, which `HandleSetEncodingsMessage` records from the
first encoding in the client's `SetEncodings`. The stock Screen Sharing
profiles never lead with `1010`, so only a client that explicitly advertises
AVC as its preferred codec (e.g. Remote Desktop Manager's ARD high-performance
profile) reaches the `0x1c` exchange. The client side also has a
FramebufferUpdate rectangle handler for encoding `1010` (`cmp w8, #0x3f2` in
the rectangle dispatcher), which is how the server delivers Message1;
replicating that rectangle's exact payload is the remaining oracle step.

### Next capture target

Repeat the oracle run with a client that advertises `1010` first (RDM or a
modified client). The oracle already records the raw `0x1c` offer and binds
the UDP ports; the remaining unconfirmed bytes are the server-reply TCP
envelope and the offer plist layout.
