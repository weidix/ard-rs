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
suite contains 3 passing tests, and the library unit suite contains 5 passing
tests. A complete all-target run is still required after the remaining work.

## Work not yet completed

The project is not yet a complete modern Screen Sharing client, and no real
desktop decode claim is made.

1. **Existing capture cannot yet be opened.** The saved type-30 capture contains
   the public exchange and encrypted records but not the native client's
   one-time internal random state. That state cannot be reconstructed from the
   public exchange. A new live run driven by the Rust type-30 implementation,
   or another explicitly authorized in-memory handoff for the same session, is
   required.
2. **Client-to-server activation is incomplete.** The exact fields and state
   transitions of `RFBSetEncryptionLevel` (`0x12`) and the later eight-byte
   control message still need to be mapped and implemented.
3. **Directional state mapping needs live confirmation.** The server-to-client
   1103 control path is established, but the outbound direction's setup and
   transition point require the same level of evidence.
4. **Decrypted payload dispatch is not integrated.** Verified record payloads
   are returned as byte vectors. The code still needs a bounded incremental
   dispatcher for the internal RFB/ARD message stream, including routing any
   encoding-1011 rectangles into the existing MVS state machine.
5. **No real framebuffer has been recovered from 1103.** There is no Rust
   output PNG from the live 492,036,896-byte capture.
6. **No native-reference comparison exists.** The required same-session native
   screenshot, dimensions, exact-pixel ratio, maximum channel error, and mean
   absolute error have not been produced.
7. **The committed structural vector is only partial.** The `0x21` fixture
   reproduces the live public structure. The 1103 tests currently use synthetic
   wrapped blocks; a minimal redacted real structural fixture is still needed.
8. **Final verification gates remain.** Full `fmt`, all-target tests, linting,
   the four requested cross-platform targets, and a feature/dependency audit
   must be rerun after end-to-end integration.

Until items 1–8 are resolved, the implementation should be described as a
confirmed and tested 1103 transport layer, not as successful real-desktop
decoding.
