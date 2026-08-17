# AVC oracle media fixtures

`oracle-diagonal-frames-1920x1080-4x272.h264` and its `.h265` peer are
synthetic Annex-B elementary streams intended as input to the AVC oracle, not
as ordinary full-frame video files.

The stream models the native remote media layout used by `ard-rs`:

- five seconds of composited 1920x1080 desktop updates at the native 60 Hz
  cadence (300 desktop frames);
- four 1920x272 codec-aligned horizontal access units per desktop frame;
- 1,200 access units in global decode order, with the last band padded by
  eight rows (only its first 264 rows are visible); the elementary-stream time
  base is 240 band AUs/s, making the 1,200 AUs exactly five seconds even though
  a raw Annex-B demuxer may expose no container-level duration field (H.264's
  VUI `time_scale` is twice its 240-picture/s rate, as required by H.264);
- one serial, low-latency prediction chain with an initial IDR/IRAP and no
  B-frames;
- 8-bit 4:2:0, video-range BT.709 output generated through VideoToolbox, the
  encoder family used by the real macOS server;
- fixed, equal-width, solid-color diagonal stripes and a centered frame number
  from `0001` through `0300` before the four-way split; only the number changes
  between desktop frames.

When packetizing, the oracle must assign each consecutive group of four access
units to four adjacent SSRCs, give the group one RTP timestamp, and increment a
single global DON (H.264 FU-B/STAP-B) or DONL (HEVC FU/AP) for every access
unit. AUD NAL units are present only to make elementary-stream access-unit
boundaries unambiguous and may be dropped by the packetizer.

Regenerate all four oracle streams from the repository root on macOS with:

```sh
./scripts/generate_oracle_fixtures.sh
```

## MVS and zlib oracle streams

`oracle-diagonal-frames-1920x1080.mvs` and
`oracle-diagonal-frames-1920x1080.zlib` contain the same fixed diagonal-stripe
background and centered frame numbers `0001` through `0300`. Each file stores
300 consecutive, full-frame rectangle payloads without RFB rectangle headers:

- MVS records are `[u32_be MVS payload length][MVS partial-update payload]`.
  The generator uses native 8x8 solid and bi-level YCbCr tiles, so diagonal
  edges and digit edges retain their exact per-pixel masks while colors use
  MVS's native 20-bit YCbCr quantization.
- zlib records are `[u32_be compressed length][deflate chunk]`. All 300 chunks
  belong to one persistent deflate stream and use XRGB8888 little-endian wire
  pixels (`B, G, R, unused`), matching encoding 6 in the oracle and decoder.

The oracle should wrap each record as a 1920x1080 rectangle with encoding 1011
for MVS or encoding 6 for zlib and schedule records at its fixed native 60 Hz
cadence; these payload files contain no clock. The single regeneration command
above creates five seconds of AVC, HEVC, MVS, and zlib and validates them
together.
