//! Development-only dump of the data a recording was made from.
//!
//! Setting `ARD_RECORD_RAW_STREAM` to any non-empty value makes a recording also
//! keep, byte for byte, the server data it was built from: the decrypted records
//! of the session's TCP stream (the RFB, MVS and zlib frames of the adaptive and
//! full profiles) and the decrypted RTP packets of the UDP media stream (the
//! AVC/HEVC video of the high-performance profiles). A session carries one path
//! or the other, depending on its quality mode.
//!
//! "Raw" here means the server's own data before the application interpreted it:
//! the record is taken after its transport protection has been removed and
//! before the parser, the decoders, the colour conversion and the encoder have
//! touched a byte of it. Nothing the application computed — no framebuffer, no
//! RGBA conversion, no NV12 planes, no encoder input — ever reaches it. Taking
//! the decrypted form rather than the ciphertext is what makes a dump replayable
//! without the session's ephemeral keys, which are never written anywhere.
//!
//! # The dump covers exactly the take
//!
//! A dump is not an independent recording: it exists to be rebuilt into a video
//! and compared, frame for frame, with the recording it belongs to. It therefore
//! starts at the take's first frame and covers the same interval the video does:
//!
//! - nothing is written before [`RawStreamSink::start_recording`];
//! - every entry carries `t`, its arrival in milliseconds since that instant, so
//!   an entry maps onto the video's timeline directly;
//! - nothing is written after [`RawStreamSink::stop_recording`], which closes
//!   every stream it opened with the take's length.
//!
//! # Layout
//!
//! Both streams write `directory`-relative files named after the server:
//! `<server> server stream.raw`/`.jsonl`, `<server> video stream.raw`/`.jsonl`,
//! `<server> video2 stream.*` and `<server> audio stream.*`. A session writes
//! only the streams it actually carries, and an idle stream leaves no file.
//!
//! A `.raw` file holds its stream's payloads back to back, with no framing of
//! its own:
//!
//! - a TCP entry is the decrypted record payload: what the server put in the
//!   record, without the `u16` length prefix the wire carried;
//! - a UDP entry is the decrypted RTP packet, without the SRTP authentication
//!   tag the wire carried.
//!
//! Its `.jsonl` twin holds one header line, one entry line per unit and one
//! footer line. An entry line carries the offset and length of its bytes in the
//! `.raw` file, `t` in milliseconds since the take's first frame, and any
//! transport-specific detail: the record sequence number for a TCP entry, or the
//! RTP sequence number and SSRC for a media packet. The header names the stream
//! and, for a media stream, the negotiated codec and payload type, which a
//! rebuild needs and which no packet repeats. The footer carries the take's
//! length, so every `t` sits inside the interval the video covers.
//!
//! # Rebuilding from a dump
//!
//! A dump is rebuilt by replaying it through the same decoders the viewer used,
//! which is what makes the result comparable with the recording it came from:
//!
//! - a record stream dump goes back through the dispatcher and the CPU decoder
//!   that decoded the RFB, MVS and zlib frames, and each frame is written at the
//!   instant its record carried;
//! - a media dump goes back through [`crate::media_stream::VideoStreamAssembler`]
//!   and the platform decoder that decoded the video, and each desktop frame is
//!   written at the instant its packets carried.
//!
//! `cargo test -p ard-viewer rebuild_from_raw_stream` does both; a media dump
//! needs a platform with an AVC/HEVC decoder.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Environment variable that enables the dump. It is only a flag: set and
/// non-empty means yes. The application reads it, because the application
/// decides which directory the dump belongs to; without it no file is ever
/// opened.
pub const ENVIRONMENT: &str = "ARD_RECORD_RAW_STREAM";

/// Buffered writer size. Payloads are usually larger than this, so the buffer
/// only keeps the small index lines from causing their own syscalls.
const WRITE_BUFFER: usize = 1 << 20;

/// One of the server-to-client streams a session can carry. Each is dumped into
/// its own pair of files, because they are independent byte streams that a
/// reader has to be able to replay separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawStreamKind {
    /// The session's TCP record stream: everything a session sends before the
    /// high-performance media modes replace the picture.
    Server,
    /// The UDP media stream that carries the AVC/HEVC video.
    Video,
    /// A second UDP video stream, when the server advertises one.
    Video2,
    /// The UDP media stream that carries audio.
    Audio,
}

impl RawStreamKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Video => "video",
            Self::Video2 => "video2",
            Self::Audio => "audio",
        }
    }
}

/// What one entry holds, as a reader has to interpret it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordFraming {
    /// A decrypted session record payload. On the wire it was a big-endian `u16`
    /// ciphertext length followed by that many bytes.
    TcpRecord,
    /// A decrypted RTP packet. On the wire it was one UDP datagram carrying the
    /// packet plus its SRTP authentication tag.
    UdpPacket,
}

impl RecordFraming {
    pub const fn label(self) -> &'static str {
        match self {
            Self::TcpRecord => "tcp-record-plaintext",
            Self::UdpPacket => "rtp-plaintext",
        }
    }
}

/// What a dumped media stream carried, from the negotiation that set it up.
///
/// A rebuild needs this to turn the dumped packets back into pictures: the codec
/// decides the decoder, and the payload type decides which packets belong to the
/// stream. It is recorded in the dump header because it is known once, at setup,
/// and the packets do not repeat it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaDetails {
    /// Negotiated codec name, as the server named it.
    pub codec: Option<String>,
    /// Negotiated RTP payload type.
    pub payload_type: Option<u8>,
    /// Codec type from the answer, when the plist exposed one.
    pub codec_type: Option<u32>,
    /// Frame size the compositor assembles the bands into, which a rebuild has
    /// to know and a packet never repeats.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl MediaDetails {
    /// The header fields this adds, or an empty string when nothing is known.
    fn header_fields(&self) -> String {
        let mut fields = String::new();
        if let Some(codec) = &self.codec {
            fields.push_str(&format!(",\"codec\":\"{}\"", self.escape(codec)));
        }
        if let Some(payload_type) = self.payload_type {
            fields.push_str(&format!(",\"payload_type\":{payload_type}"));
        }
        if let Some(codec_type) = self.codec_type {
            fields.push_str(&format!(",\"codec_type\":{codec_type}"));
        }
        if let Some(width) = self.width {
            fields.push_str(&format!(",\"width\":{width}"));
        }
        if let Some(height) = self.height {
            fields.push_str(&format!(",\"height\":{height}"));
        }
        fields
    }

    /// A codec name reaches JSON, so it cannot carry a quote or a backslash.
    fn escape(&self, value: &str) -> String {
        value
            .chars()
            .filter(|character| !matches!(character, '"' | '\\'))
            .collect()
    }
}

/// One stream's dump: the `.raw` file and its `.jsonl` index.
///
/// The files are opened lazily, on the first entry, so a stream that never
/// carries anything leaves nothing behind.
#[derive(Debug)]
pub struct RawStreamDump {
    kind: RawStreamKind,
    directory: PathBuf,
    server: String,
    /// Which take of this application this dump belongs to, counted from one.
    take: u64,
    details: MediaDetails,
    files: Option<Files>,
    records: u64,
    bytes: u64,
}

#[derive(Debug)]
struct Files {
    raw: BufWriter<File>,
    index: BufWriter<File>,
    raw_path: PathBuf,
    index_path: PathBuf,
}

impl RawStreamDump {
    /// A dump that will write `<directory>/<server> <kind> stream.*` once it has
    /// something to write.
    pub fn new(directory: &Path, server: &str, kind: RawStreamKind, take: u64) -> Self {
        Self {
            kind,
            directory: directory.to_path_buf(),
            server: sanitize_server_name(server),
            take,
            details: MediaDetails::default(),
            files: None,
            records: 0,
            bytes: 0,
        }
    }

    /// Describe what this stream carries, before its first entry is written.
    ///
    /// Only a media stream has details to describe, and only the negotiation
    /// knows them, so the caller sets them once the stream is set up.
    pub fn describe(&mut self, details: MediaDetails) {
        self.details = details;
    }

    pub fn media_details(&self) -> &MediaDetails {
        &self.details
    }

    /// Append one decrypted unit, opening the files on first use.
    ///
    /// `t` is the unit's arrival in milliseconds since the take's first frame, so
    /// an entry lines up with the video's timeline. `details` is appended
    /// verbatim to the entry line as additional JSON fields, or is empty.
    pub fn append(
        &mut self,
        sequence: u32,
        t: u64,
        payload: &[u8],
        framing: RecordFraming,
        details: &str,
    ) -> Result<(), String> {
        self.open()?;
        let offset = self.bytes;
        let length = payload.len() as u64;
        self.bytes = self.bytes.saturating_add(length);
        self.records = self.records.saturating_add(1);
        let line = format!(
            "{{\"sequence\":{sequence},\"offset\":{offset},\"length\":{length},\"framed\":\"{}\",\"t\":{t}{details}}}",
            framing.label(),
        );
        let files = self.files.as_mut().expect("the files were just opened");
        files
            .raw
            .write_all(payload)
            .map_err(|error| format!("无法写入裸流：{error}"))?;
        write_line(files, &line)
    }

    fn open(&mut self) -> Result<&mut Files, String> {
        if self.files.is_none() {
            self.files = Some(self.create()?);
        }
        Ok(self.files.as_mut().expect("the files were just opened"))
    }

    fn create(&self) -> Result<Files, String> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|error| format!("无法创建裸流目录 {}：{error}", self.directory.display()))?;
        // The first take of a recording series keeps the plain name; later ones
        // carry their number, so a second take cannot silently replace the first
        // take's dump the way its `.mp4` does not.
        let stem = format!(
            "{} {} stream{}",
            self.server,
            self.kind.label(),
            self.take_suffix()
        );
        let raw_path = self.directory.join(format!("{stem}.raw"));
        let index_path = self.directory.join(format!("{stem}.jsonl"));
        let raw = File::create(&raw_path)
            .map_err(|error| format!("无法创建裸流文件 {}：{error}", raw_path.display()))?;
        let index = File::create(&index_path)
            .map_err(|error| format!("无法创建裸流索引 {}：{error}", index_path.display()))?;
        let mut files = Files {
            raw: BufWriter::with_capacity(WRITE_BUFFER, raw),
            index: BufWriter::with_capacity(WRITE_BUFFER, index),
            raw_path,
            index_path,
        };
        write_line(
            &mut files,
            &format!(
                "{{\"ard_raw_stream\":2,\"source\":\"server\",\"stream\":\"{}\",\"content\":\"decrypted\"{}}}",
                self.kind.label(),
                self.details.header_fields(),
            ),
        )?;
        Ok(files)
    }

    /// The `.raw` file, once the dump has something to write. Before that the
    /// path is only planned, so nothing is created for an idle stream.
    pub fn raw_path(&self) -> Option<&Path> {
        self.files.as_ref().map(|files| files.raw_path.as_path())
    }

    /// The index file, once the dump has something to write.
    pub fn index_path(&self) -> Option<&Path> {
        self.files.as_ref().map(|files| files.index_path.as_path())
    }

    /// Path the `.raw` file will have, whether or not it exists yet.
    pub fn planned_raw_path(&self) -> PathBuf {
        self.directory.join(format!(
            "{} {} stream{}.raw",
            self.server,
            self.kind.label(),
            self.take_suffix()
        ))
    }

    /// The take number as it appears in a file name: nothing for the first take.
    fn take_suffix(&self) -> String {
        if self.take > 1 {
            format!("-{}", self.take)
        } else {
            String::new()
        }
    }

    pub fn kind(&self) -> RawStreamKind {
        self.kind
    }

    pub fn entries(&self) -> u64 {
        self.records
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes
    }

    /// Flush the dump and write its footer. Finishing twice is harmless; a dump
    /// that never received anything writes nothing at all.
    ///
    /// `take_ms` is the take's length in milliseconds, which is the interval the
    /// entries cover: they all sit within `0..=take_ms`.
    pub fn finish(&mut self, take_ms: u64) -> Result<(), String> {
        let Some(files) = self.files.as_mut() else {
            return Ok(());
        };
        let line = format!(
            "{{\"end\":true,\"records\":{},\"bytes\":{},\"take_ms\":{take_ms}}}",
            self.records, self.bytes
        );
        write_line(files, &line)?;
        files
            .raw
            .flush()
            .map_err(|error| format!("无法刷新裸流：{error}"))?;
        files
            .index
            .flush()
            .map_err(|error| format!("无法刷新裸流索引：{error}"))
    }
}

// A dump is closed by `RawStreamSink::stop_recording`, which knows how long the
// take was and writes that into the footer. Dropping one without closing it
// (a `panic`, or a sink discarded mid-take) still leaves a readable index: the
// buffered writers flush on drop, and only the footer is missing.

fn write_line(files: &mut Files, line: &str) -> Result<(), String> {
    files
        .index
        .write_all(line.as_bytes())
        .and_then(|()| files.index.write_all(b"\n"))
        .map_err(|error| format!("无法写入裸流索引：{error}"))
}

/// The shared dump handle the application attaches to a session and arms for the
/// duration of a take.
///
/// One sink serves every server-to-client path of a connection: the client
/// records the TCP record stream, and the media receiver records its UDP
/// packets, each into its own pair of files behind the same lock. Sharing the
/// sink is what keeps a take's dump consistent when both paths are alive at once,
/// and what lets a caller who has neither still hold one.
///
/// The sink is inert until [`RawStreamSink::start_recording`] arms it, so a dump
/// covers exactly the take and not the session around it.
#[derive(Debug)]
pub struct RawStreamSink {
    directory: PathBuf,
    server: String,
    streams: Mutex<Streams>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStreamError {
    pub stream: RawStreamKind,
    pub message: String,
}

#[derive(Debug, Default)]
struct Streams {
    server: Option<RawStreamDump>,
    video: Option<RawStreamDump>,
    video2: Option<RawStreamDump>,
    audio: Option<RawStreamDump>,
    failure: Option<RawStreamError>,
    /// The take's time origin: `None` while no take is being dumped, which is
    /// what keeps a dump inside the recording's interval.
    take: Option<Take>,
    /// How many takes this sink has been armed for, so a later take writes its
    /// own files instead of replacing an earlier take's.
    takes: u64,
}

/// A take being dumped.
///
/// The origin is shared with whoever writes the recording, because the take's
/// zero is the recording's first frame, which the dump is not the one to see
/// first. Until that frame arrives the take is armed but not measurable, and
/// nothing is written.
#[derive(Debug, Clone, Default)]
pub struct TakeOrigin(Arc<OnceLock<Instant>>);

impl TakeOrigin {
    /// An origin that will be filled in with the recording's first frame.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the take's zero.
    pub fn set(&self, origin: Instant) {
        let _ = self.0.set(origin);
    }

    /// The take's zero, once the recording's first frame has arrived.
    pub fn instant(&self) -> Option<Instant> {
        self.0.get().copied()
    }

    fn elapsed_ms(&self) -> Option<u64> {
        self.instant()
            .map(|origin| origin.elapsed().as_millis() as u64)
    }
}

#[derive(Debug)]
struct Take {
    origin: TakeOrigin,
    /// Counted from one, and part of the dump's file name after the first.
    number: u64,
}

impl RawStreamSink {
    /// A sink that dumps into `directory`, named after `server`.
    pub fn new(directory: impl Into<PathBuf>, server: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            directory: directory.into(),
            server: server.into(),
            streams: Mutex::new(Streams::default()),
        })
    }

    /// A sink when the environment asks for one, in `directory`.
    ///
    /// Unset or blank leaves the caller without a dump, which is the normal
    /// case. The sink is not armed: the caller starts it with the take.
    pub fn from_environment(
        directory: impl Into<PathBuf>,
        server: impl Into<String>,
    ) -> Option<Arc<Self>> {
        let value = std::env::var(ENVIRONMENT).ok()?;
        if value.trim().is_empty() {
            return None;
        }
        Some(Self::new(directory, server))
    }

    /// Whether a take is armed, so a caller can tell that what it records now
    /// is meant to be replayable on its own.
    pub fn writes_a_take(&self) -> bool {
        self.streams
            .lock()
            .map(|streams| streams.take.is_some())
            .unwrap_or(false)
    }

    /// Arm the dump at the instant the recording's first frame was presented.
    ///
    /// `origin` is that instant, which is also the zero of the recorded video's
    /// timeline: every entry's `t` is measured from it, so a dump entry and a
    /// video frame carry the same clock. Nothing before this call is written.
    pub fn start_recording(&self, origin: TakeOrigin) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.takes = streams.takes.saturating_add(1);
            streams.take = Some(Take {
                origin,
                number: streams.takes,
            });
        }
    }

    /// Disarm the dump and close every stream it opened.
    ///
    /// The footer of each stream records the take's length, so a reader knows
    /// the interval the entries cover. Stopping twice, or stopping without
    /// starting, is harmless.
    pub fn stop_recording(&self) -> Vec<RawStreamReport> {
        let mut reports = Vec::new();
        let Ok(mut streams) = self.streams.lock() else {
            return reports;
        };
        let Some(take) = streams.take.take() else {
            return reports;
        };
        // A take whose first frame never arrived wrote nothing, so there is no
        // interval to close.
        let Some(take_ms) = take.origin.elapsed_ms() else {
            return reports;
        };
        for kind in [
            RawStreamKind::Server,
            RawStreamKind::Video,
            RawStreamKind::Video2,
            RawStreamKind::Audio,
        ] {
            if let Some(dump) = Self::slot(&mut streams, kind).as_mut() {
                reports.push(RawStreamReport {
                    kind,
                    raw_path: dump
                        .raw_path()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| dump.planned_raw_path()),
                    entries: dump.entries(),
                    bytes: dump.total_bytes(),
                });
                if let Err(message) = dump.finish(take_ms)
                    && streams.failure.is_none()
                {
                    eprintln!("ard-core: 裸流结束失败：{message}");
                    streams.failure = Some(RawStreamError {
                        stream: kind,
                        message,
                    });
                }
            }
            *Self::slot(&mut streams, kind) = None;
        }
        reports
    }

    /// Describe what a media stream carries, before its first packet arrives.
    ///
    /// The codec and payload type are agreed once, during setup, and the packets
    /// do not repeat them, so the dump header has to carry them for a rebuild.
    pub fn describe_stream(&self, kind: RawStreamKind, details: MediaDetails) {
        let Ok(mut streams) = self.streams.lock() else {
            return;
        };
        let server = self.server.clone();
        let directory = self.directory.clone();
        let take_number = streams.take.as_ref().map(|take| take.number).unwrap_or(1);
        let dump = match Self::slot(&mut streams, kind) {
            Some(dump) => dump,
            None => {
                let dump = RawStreamDump::new(&directory, &server, kind, take_number);
                Self::slot(&mut streams, kind).insert(dump)
            }
        };
        dump.describe(details);
    }

    /// Whether a take is being dumped right now.
    pub fn is_recording(&self) -> bool {
        self.streams
            .lock()
            .map(|streams| streams.take.is_some())
            .unwrap_or(false)
    }

    /// Record one decrypted TCP record payload.
    ///
    /// The payload is the record after its authentication and decryption and
    /// before the parser sees it. Nothing is recorded outside a take. A dump
    /// failure is recorded and reported once per stream; it must not interrupt
    /// the session.
    pub fn record_tcp(&self, sequence: u32, payload: &[u8]) {
        self.record(RawStreamKind::Server, sequence, payload, "");
    }

    /// Record one decrypted RTP packet of a media stream.
    ///
    /// `rtp_sequence` and `ssrc` are the packet's own header fields, which a
    /// reader needs to place it in its stream. Nothing is recorded outside a
    /// take.
    pub fn record_udp(
        &self,
        kind: RawStreamKind,
        sequence: u32,
        payload: &[u8],
        rtp_sequence: u16,
        ssrc: u32,
    ) {
        let details = format!(",\"rtp\":{rtp_sequence},\"ssrc\":{ssrc}");
        self.record(kind, sequence, payload, &details);
    }

    /// The first failure of one stream, if any.
    pub fn failure(&self) -> Option<RawStreamError> {
        self.streams.lock().ok()?.failure.clone()
    }

    /// Where a stream's `.raw` file is (or will be).
    pub fn raw_path(&self, kind: RawStreamKind) -> PathBuf {
        let Ok(mut streams) = self.streams.lock() else {
            return PathBuf::new();
        };
        if let Some(path) = Self::slot(&mut streams, kind)
            .as_ref()
            .and_then(RawStreamDump::raw_path)
        {
            return path.to_path_buf();
        }
        let take = streams.take.as_ref().map(|take| take.number).unwrap_or(1);
        RawStreamDump::new(&self.directory, &self.server, kind, take).planned_raw_path()
    }

    fn record(&self, kind: RawStreamKind, sequence: u32, payload: &[u8], details: &str) {
        let Ok(mut streams) = self.streams.lock() else {
            return;
        };
        if streams.failure.is_some() {
            return;
        }
        // The take gates the dump: a session that is not being recorded writes
        // nothing, which is what keeps a dump inside the recording's interval.
        let Some((t, take_number)) = streams
            .take
            .as_ref()
            .and_then(|take| take.origin.elapsed_ms().map(|t| (t, take.number)))
        else {
            return;
        };
        // The name and directory are cloned only when a stream's dump is first
        // created: a media session records thousands of datagrams, and each one
        // must not pay for two allocations.
        let dump = match Self::slot(&mut streams, kind) {
            Some(dump) => dump,
            None => {
                let dump = RawStreamDump::new(&self.directory, &self.server, kind, take_number);
                Self::slot(&mut streams, kind).insert(dump)
            }
        };
        let framing = match kind {
            RawStreamKind::Server => RecordFraming::TcpRecord,
            RawStreamKind::Video | RawStreamKind::Video2 | RawStreamKind::Audio => {
                RecordFraming::UdpPacket
            }
        };
        if let Err(message) = dump.append(sequence, t, payload, framing, details) {
            eprintln!("ard-core: 写入裸流失败，已停止记录：{message}");
            streams.failure = Some(RawStreamError {
                stream: kind,
                message,
            });
            *Self::slot(&mut streams, kind) = None;
        }
    }

    fn slot(streams: &mut Streams, kind: RawStreamKind) -> &mut Option<RawStreamDump> {
        match kind {
            RawStreamKind::Server => &mut streams.server,
            RawStreamKind::Video => &mut streams.video,
            RawStreamKind::Video2 => &mut streams.video2,
            RawStreamKind::Audio => &mut streams.audio,
        }
    }
}

/// What one stream of a finished take wrote.
#[derive(Debug, Clone)]
pub struct RawStreamReport {
    pub kind: RawStreamKind,
    /// Path of the stream's `.raw` file, whether or not it was created.
    pub raw_path: PathBuf,
    pub entries: u64,
    pub bytes: u64,
}

/// One entry of a dump index: where its bytes live and when they arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawStreamRecord {
    pub sequence: u32,
    pub offset: u64,
    pub length: u64,
    pub framing: RecordFraming,
    /// Arrival in milliseconds since the take's first frame, which is the same
    /// clock the recorded video's timeline uses.
    pub t: u64,
    /// RTP sequence number, for a media packet that carried one.
    pub rtp_sequence: Option<u16>,
    /// RTP synchronization source, for a media packet that carried one.
    pub ssrc: Option<u32>,
}

/// A dump index, as a reader needs it: the header line, every entry and the
/// take's length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStreamIndex {
    /// The header line, verbatim: a JSON object that names the format.
    pub header: String,
    pub records: Vec<RawStreamRecord>,
    /// Length of the take the entries belong to, in milliseconds: every entry's
    /// `t` is within `0..=take_ms`. `None` for a dump that was never closed.
    pub take_ms: Option<u64>,
}

impl RawStreamIndex {
    /// Read a dump index. The entries' bytes are read separately out of the
    /// `.raw` file [`Self::raw_path`] names.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("无法读取裸流索引 {}：{error}", path.display()))?;
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let header = lines
            .next()
            .ok_or_else(|| "裸流索引是空的".to_owned())?
            .to_owned();
        if !header.contains("\"ard_raw_stream\"") {
            return Err("裸流索引头缺少格式标记".into());
        }
        let mut records = Vec::new();
        let mut take_ms = None;
        for line in lines {
            if line.contains("\"end\"") {
                take_ms = number(line, "\"take_ms\"");
                continue;
            }
            records.push(RawStreamRecord {
                sequence: number(line, "\"sequence\"").unwrap_or(0) as u32,
                offset: number(line, "\"offset\"").ok_or("记录缺少 offset")?,
                length: number(line, "\"length\"").ok_or("记录缺少 length")?,
                framing: if line.contains("\"framed\":\"rtp-plaintext\"") {
                    RecordFraming::UdpPacket
                } else {
                    RecordFraming::TcpRecord
                },
                t: number(line, "\"t\"").unwrap_or(0),
                rtp_sequence: number(line, "\"rtp\"").map(|value| value as u16),
                ssrc: number(line, "\"ssrc\"").map(|value| value as u32),
            });
        }
        Ok(Self {
            header,
            records,
            take_ms,
        })
    }

    /// Path of the `.raw` file an index at `path` describes.
    pub fn raw_path(path: &Path) -> PathBuf {
        path.with_extension("raw")
    }
}

/// Read the unsigned integer that follows `key` in one index line. Every value
/// in the format is a plain integer, so no JSON parser is needed.
fn number(line: &str, key: &str) -> Option<u64> {
    let rest = line.split_once(key)?.1;
    let rest = rest.strip_prefix(':')?;
    let digits = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

/// Reduce a server address to something safe to use as a file name.
///
/// A dump is named after the server it came from. A host may arrive as a
/// bracketed IPv6 literal or with a port, and no part of it may escape the
/// directory the caller chose, so everything but the characters a host name is
/// made of becomes a dash. The result is never empty, so a dump always has a
/// name.
fn sanitize_server_name(server: &str) -> String {
    let mut name: String = server
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_owned();
    name.truncate(64);
    if name.is_empty() {
        "server".to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{
        RawStreamDump, RawStreamIndex, RawStreamKind, RawStreamSink, RecordFraming, TakeOrigin,
    };

    fn test_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ard-record-stream-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&directory).ok();
        std::fs::create_dir_all(&directory).expect("temp directory");
        directory
    }

    #[test]
    fn a_dump_holds_the_decrypted_records_in_arrival_order() {
        let directory = test_directory("order");
        let first = [1_u8, 2, 3, 4];
        let second = [9_u8; 48];

        let mut dump = RawStreamDump::new(&directory, "192.0.2.10", RawStreamKind::Server, 1);
        assert!(dump.raw_path().is_none(), "nothing is created before use");
        dump.append(7, 0, &first, RecordFraming::TcpRecord, "")
            .expect("first record");
        dump.append(8, 40, &second, RecordFraming::TcpRecord, "")
            .expect("second record");
        dump.finish(120).expect("dump finishes");

        // The raw file is the payload bytes and nothing else: no length prefix,
        // no authentication tag, no parser output.
        let raw_path = dump.raw_path().expect("the dump has a path").to_path_buf();
        assert_eq!(raw_path, directory.join("192.0.2.10 server stream.raw"));
        let raw = std::fs::read(&raw_path).expect("raw exists");
        assert_eq!(&raw[..4], &first);
        assert_eq!(&raw[4..], &second);

        let index = RawStreamIndex::read(&directory.join("192.0.2.10 server stream.jsonl"))
            .expect("index readable");
        assert!(index.header.contains("\"ard_raw_stream\":2"));
        assert!(index.header.contains("\"content\":\"decrypted\""));
        assert_eq!(index.take_ms, Some(120));
        assert_eq!(index.records.len(), 2);
        assert_eq!(index.records[0].sequence, 7);
        assert_eq!(index.records[0].t, 0);
        assert_eq!(index.records[0].offset, 0);
        assert_eq!(index.records[0].length, 4);
        assert_eq!(index.records[0].framing, RecordFraming::TcpRecord);
        assert_eq!(index.records[1].sequence, 8);
        assert_eq!(index.records[1].t, 40);
        assert_eq!(index.records[1].offset, 4);
        assert_eq!(index.records[1].length, 48);
        assert!(index.records[1].ssrc.is_none());
    }

    #[test]
    fn a_video_dump_holds_decrypted_packets_with_their_rtp_position() {
        let directory = test_directory("video");
        let first = [0x80_u8, 0x60, 0x00, 0x2a, 0, 0, 0, 1, 0xde, 0xad];
        let second = [0x80_u8, 0x60, 0x00, 0x2b, 0, 0, 0, 2, 0xbe, 0xef];

        let mut dump = RawStreamDump::new(&directory, "192.0.2.10:5900", RawStreamKind::Video, 1);
        dump.append(
            0,
            16,
            &first,
            RecordFraming::UdpPacket,
            ",\"rtp\":42,\"ssrc\":4001",
        )
        .expect("first packet");
        dump.append(
            1,
            32,
            &second,
            RecordFraming::UdpPacket,
            ",\"rtp\":43,\"ssrc\":4001",
        )
        .expect("second packet");
        dump.finish(64).expect("dump finishes");

        // The packet is stored whole, because a replay has to send the same RTP
        // packet again.
        let raw =
            std::fs::read(directory.join("192.0.2.10-5900 video stream.raw")).expect("raw exists");
        assert_eq!(raw, [first.as_slice(), second.as_slice()].concat());

        let index = RawStreamIndex::read(&directory.join("192.0.2.10-5900 video stream.jsonl"))
            .expect("index readable");
        assert!(index.header.contains("\"stream\":\"video\""));
        assert_eq!(index.take_ms, Some(64));
        assert_eq!(index.records[0].framing, RecordFraming::UdpPacket);
        assert_eq!(index.records[0].t, 16);
        assert_eq!(index.records[0].rtp_sequence, Some(42));
        assert_eq!(index.records[0].ssrc, Some(4001));
        assert_eq!(index.records[1].t, 32);
        assert_eq!(index.records[1].rtp_sequence, Some(43));
    }

    #[test]
    fn a_stream_that_carries_nothing_writes_nothing() {
        let directory = test_directory("idle");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");
        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        sink.start_recording(origin);
        sink.stop_recording();
        assert_eq!(
            std::fs::read_dir(&directory)
                .expect("directory readable")
                .count(),
            0,
            "an idle stream leaves no files behind"
        );
    }

    #[test]
    fn the_take_gates_the_dump() {
        let directory = test_directory("gated");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");

        // Nothing before the recording: the dump covers the take, not the
        // session that surrounds it.
        sink.record_tcp(1, &[1, 2, 3]);
        assert!(!sink.is_recording());
        assert!(
            !directory.join("192.0.2.10 server stream.raw").exists(),
            "a session that is not being recorded writes nothing"
        );

        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        sink.start_recording(origin);
        assert!(sink.is_recording());
        sink.record_tcp(2, &[4, 5, 6]);
        std::thread::sleep(Duration::from_millis(15));
        sink.record_tcp(3, &[7, 8, 9]);
        sink.stop_recording();

        // Nothing after the recording either.
        sink.record_tcp(4, &[10]);
        assert!(!sink.is_recording());

        let index = RawStreamIndex::read(&directory.join("192.0.2.10 server stream.jsonl"))
            .expect("index readable");
        assert_eq!(
            index.records.len(),
            2,
            "only the take's records were dumped"
        );
        assert_eq!(index.records[0].sequence, 2);
        assert_eq!(index.records[1].sequence, 3);
        // The two entries are ordered and the second is later on the take's
        // clock, which is the video's clock.
        assert!(index.records[1].t >= index.records[0].t);
        assert!(index.records[1].t >= 15);
        let take_ms = index.take_ms.expect("the footer records the take");
        assert!(index.records.iter().all(|record| record.t <= take_ms));
        assert!(take_ms >= 15);
    }

    #[test]
    fn a_finished_take_does_not_append_after_its_footer() {
        let directory = test_directory("finished");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");
        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        sink.start_recording(origin);
        sink.record_tcp(1, &[1, 2, 3]);
        sink.stop_recording();
        // Stopping twice is harmless and a later record cannot reopen the file
        // behind a written footer.
        sink.stop_recording();
        sink.record_tcp(2, &[4]);

        let text = std::fs::read_to_string(directory.join("192.0.2.10 server stream.jsonl"))
            .expect("index readable");
        assert_eq!(text.matches("\"end\":true").count(), 1);
        let index = RawStreamIndex::read(&directory.join("192.0.2.10 server stream.jsonl"))
            .expect("index readable");
        assert_eq!(index.records.len(), 1);
    }

    #[test]
    fn one_sink_serves_every_stream_of_a_take() {
        let directory = test_directory("sink");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");
        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        sink.start_recording(origin);
        sink.record_tcp(1, &[1, 2, 3, 4]);
        sink.record_udp(RawStreamKind::Video, 0, &[0x80, 0x60, 0x00, 0x2a], 42, 4001);
        sink.record_udp(RawStreamKind::Video, 1, &[0x80, 0x65], 43, 4001);
        assert!(sink.failure().is_none());
        sink.stop_recording();
        drop(sink);

        // Each stream is a pair of files of its own, so a reader can rebuild
        // them separately.
        assert!(directory.join("192.0.2.10 server stream.raw").exists());
        assert!(directory.join("192.0.2.10 video stream.raw").exists());
        assert!(!directory.join("192.0.2.10 audio stream.raw").exists());

        let video = RawStreamIndex::read(&directory.join("192.0.2.10 video stream.jsonl"))
            .expect("video index readable");
        assert_eq!(video.records.len(), 2);
        assert_eq!(video.records[0].rtp_sequence, Some(42));
        assert_eq!(video.records[1].rtp_sequence, Some(43));
        assert_eq!(video.records[1].ssrc, Some(4001));

        let server = RawStreamIndex::read(&directory.join("192.0.2.10 server stream.jsonl"))
            .expect("server index readable");
        assert_eq!(server.records.len(), 1);
    }

    #[test]
    fn a_dumped_interval_holds_exactly_the_bytes_that_arrived() {
        // The question a dump has to answer is whether its byte intervals cover
        // exactly the stream that arrived, in order and with nothing inserted.
        // Read every entry back by its own offset and length and compare it with
        // the bytes that were appended.
        let directory = test_directory("intervals");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");
        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        sink.start_recording(origin);
        let expected: Vec<Vec<u8>> = (0..64_u8)
            .map(|index| vec![index; 1 + usize::from(index) * 3])
            .collect();
        for payload in &expected {
            sink.record_tcp(0, payload);
        }
        sink.stop_recording();

        let raw_path = directory.join("192.0.2.10 server stream.raw");
        let raw = std::fs::read(&raw_path).expect("raw exists");
        let index = RawStreamIndex::read(&directory.join("192.0.2.10 server stream.jsonl"))
            .expect("index readable");

        assert_eq!(index.records.len(), expected.len());
        let mut covered = 0_u64;
        for (number, (entry, payload)) in index.records.iter().zip(&expected).enumerate() {
            // The intervals are contiguous from the start of the file, so an
            // entry's bytes are exactly `offset..offset + length`.
            assert_eq!(entry.offset, covered, "entry {number} leaves a hole");
            assert_eq!(entry.length as usize, payload.len());
            let start = entry.offset as usize;
            let end = start + entry.length as usize;
            assert_eq!(
                &raw[start..end],
                payload.as_slice(),
                "entry {number} does not hold the bytes that arrived"
            );
            covered = end as u64;
        }
        assert_eq!(covered, raw.len() as u64, "the intervals cover the file");
        assert_eq!(raw.len(), expected.iter().map(Vec::len).sum::<usize>());
    }

    #[test]
    fn a_second_take_writes_its_own_files() {
        // A recording series keeps every take, so a dump must not replace the
        // previous take's files.
        let directory = test_directory("takes");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");

        let first = TakeOrigin::new();
        first.set(Instant::now());
        sink.start_recording(first);
        sink.record_tcp(1, &[1, 2, 3]);
        let reports = sink.stop_recording();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].entries, 1);
        assert_eq!(reports[0].bytes, 3);
        let first_path = reports[0].raw_path.clone();
        assert_eq!(first_path, directory.join("192.0.2.10 server stream.raw"));

        let second = TakeOrigin::new();
        second.set(Instant::now());
        sink.start_recording(second);
        sink.record_tcp(1, &[4, 5, 6, 7]);
        let reports = sink.stop_recording();
        assert_eq!(reports.len(), 1);
        let second_path = reports[0].raw_path.clone();
        assert_eq!(
            second_path,
            directory.join("192.0.2.10 server stream-2.raw")
        );
        assert!(first_path.exists(), "the first take's dump is still there");
        assert_eq!(std::fs::read(&second_path).expect("readable"), [4, 5, 6, 7]);
        assert_eq!(std::fs::read(&first_path).expect("readable"), [1, 2, 3]);
    }

    #[test]
    fn a_take_that_never_started_reports_nothing() {
        let directory = test_directory("unstarted");
        let sink = RawStreamSink::new(&directory, "192.0.2.10");
        // Armed, but the recording never produced a first frame.
        sink.start_recording(TakeOrigin::new());
        sink.record_tcp(1, &[1, 2, 3]);
        assert!(sink.stop_recording().is_empty());
        assert_eq!(
            std::fs::read_dir(&directory)
                .expect("directory readable")
                .count(),
            0,
            "a take that never began leaves no file"
        );
    }

    #[test]
    fn a_server_address_becomes_a_file_name() {
        assert_eq!(super::sanitize_server_name("192.0.2.10"), "192.0.2.10");
        assert_eq!(
            super::sanitize_server_name("[fe80::1]:5900"),
            "fe80--1--5900"
        );
        assert_eq!(super::sanitize_server_name("../escape"), "..-escape");
        assert_eq!(super::sanitize_server_name(""), "server");
        assert_eq!(super::sanitize_server_name("--"), "server");
    }

    #[test]
    fn a_sink_is_named_after_the_server_it_came_from() {
        let directory = test_directory("named");
        // A host that arrived with a port still names the dump, and the address
        // cannot climb out of the directory the caller chose.
        let sink = RawStreamSink::new(&directory, "192.0.2.10:5900");
        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        sink.start_recording(origin);
        sink.record_tcp(1, &[0_u8; 16]);
        assert_eq!(
            sink.raw_path(RawStreamKind::Server),
            directory.join("192.0.2.10-5900 server stream.raw")
        );
        sink.stop_recording();

        let hostile = RawStreamSink::new(&directory, "../outside");
        let origin = TakeOrigin::new();
        origin.set(Instant::now());
        hostile.start_recording(origin);
        hostile.record_tcp(1, &[0_u8; 16]);
        assert_eq!(
            hostile.raw_path(RawStreamKind::Server),
            directory.join("..-outside server stream.raw")
        );
        hostile.stop_recording();
    }
}
