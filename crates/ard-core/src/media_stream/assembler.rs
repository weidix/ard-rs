//! RTP ordering and access-unit assembly for the AVC/HEVC media stream.
//!
//! Apple's media stream codes one desktop frame as up to four pictures — one per
//! horizontal band — carried by four adjacent RTP streams (SSRCs) that share a
//! single serial prediction chain. A decoder may only be handed those pictures
//! in the chain's own order, so a receiver cannot decode "whatever completed
//! last": it has to reorder packets inside each stream, wait for a whole sampling
//! instant, and release the bands in decode order.
//!
//! That machine lives here, apart from sockets and SRTP, because two callers need
//! it and they must behave identically:
//!
//! - the live receiver in [`super::udp`], which decapsulates packets off the
//!   socket and hands the decrypted ones in;
//! - the rebuild path, which reads a dump of the same decrypted packets from a
//!   file and has to reassemble them exactly as the live session did, or a
//!   rebuilt video could not be compared with the recording it came from.
//!
//! [`VideoStreamAssembler`] owns the per-stream reorder buffers and
//! depacketizers, the cross-stream batching, and the sync-recovery state. It
//! reports what it did — completed batches, losses, and whether a recovery is
//! wanted — so a live caller can send RTCP feedback and a rebuild can simply
//! keep reading.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::{Error, Result};

use super::negotiation::MediaStreamCodec;
use super::rtp::{AccessUnit, H264Depacketizer, HevcDepacketizer, RtpPacket, RtpReorderBuffer};

/// Native AVC partitions one desktop frame into four horizontal slices. Each
/// slice uses an adjacent SSRC with independent SRTP/RTP state, while all four
/// share one interleaved video decoder reference chain.
pub const AVC_VIDEO_SLICE_COUNT: usize = 4;

/// Bound cross-SSRC decoding-order reassembly. This is a loss detector, not a
/// presentation timer: a sparse timestamp is released only when the following
/// decode-order number proves its end.
const MAX_PENDING_FRAME_BATCHES: usize = 8;
const MAX_TRACKED_RTP_TIMESTAMPS: usize = 64;

/// One desktop sampling instant, assembled from the bands that carried it.
#[derive(Debug)]
pub struct AssembledFrame {
    /// RTP timestamp shared by every access unit in the frame.
    pub timestamp: u32,
    /// The frame's access units, in decode order.
    pub access_units: Vec<(usize, AccessUnit)>,
    /// Local monotonic time immediately after the first packet for any access
    /// unit in this sampling instant was received.
    pub first_packet_received_at: Instant,
    /// Local monotonic time at which the first access unit was completed.
    pub first_access_unit_completed_at: Instant,
}

/// One access unit with the instants it was received and completed at.
#[derive(Debug)]
struct TimedAccessUnit {
    unit: AccessUnit,
    first_packet_received_at: Instant,
    completed_at: Instant,
}

/// What one pushed packet did.
#[derive(Debug, Default)]
pub struct PushOutcome {
    /// Access units the reorder buffer gave up, because a later one proved the
    /// earlier gap would never be filled. Non-zero means the caller should ask
    /// the server for a fresh keyframe.
    pub losses: usize,
    /// The reorder buffer dropped whole timestamps, which is a stream-level loss
    /// rather than a single gap.
    pub dropped_timestamps: bool,
}

/// How an assembled frame ended a push.
#[derive(Debug, Default)]
pub struct AssembleOutcome {
    /// The frame that became complete and decodable, when one did.
    pub frame: Option<AssembledFrame>,
    /// Packets discarded because their decode order was already released or a
    /// duplicate band arrived.
    pub ignored_units: usize,
    /// The prediction chain was violated, so the caller must recover. It is a
    /// silent recovery: no stream error was raised, but the decoder needs a new
    /// keyframe.
    pub chain_reset: bool,
    /// The caller must recover and the stream is not decodable as it stands.
    pub stream_error: bool,
}

/// Per-stream receive state: reorder, depacketize, remember arrival times.
struct InboundStream {
    reorder: RtpReorderBuffer,
    depacketizer: VideoDepacketizer,
    completed: VecDeque<TimedAccessUnit>,
    first_packet_arrivals: VecDeque<(u32, Instant)>,
    next_sequence: Option<u16>,
    damaged_timestamp: Option<u32>,
}

impl InboundStream {
    fn new(codec: MediaStreamCodec) -> Self {
        Self {
            reorder: RtpReorderBuffer::with_codec(codec),
            depacketizer: VideoDepacketizer::new(codec),
            completed: VecDeque::new(),
            first_packet_arrivals: VecDeque::new(),
            next_sequence: None,
            damaged_timestamp: None,
        }
    }

    fn reset_pending(&mut self) {
        self.depacketizer.reset();
        self.reorder.reset_pending();
        self.completed.clear();
        self.first_packet_arrivals.clear();
        self.damaged_timestamp = None;
    }

    /// Feed one decrypted RTP packet. `received_at` is the instant it arrived,
    /// which a live caller takes from the socket and a rebuild invents, because
    /// only the order matters for reassembly.
    fn push(&mut self, packet: &[u8], received_at: Instant) -> Result<PushOutcome> {
        let packet_timestamp = RtpPacket::parse(packet)?.header.timestamp;
        if !self
            .first_packet_arrivals
            .iter()
            .any(|(timestamp, _)| *timestamp == packet_timestamp)
        {
            if self.first_packet_arrivals.len() >= MAX_TRACKED_RTP_TIMESTAMPS {
                return Err(Error::LimitExceeded("RTP timestamp timing tracker"));
            }
            self.first_packet_arrivals
                .push_back((packet_timestamp, received_at));
        }
        let ready_packets = self.reorder.push(packet)?;
        let mut losses = self.reorder.take_dropped_access_units();
        let dropped_timestamps = losses != 0;
        if dropped_timestamps {
            self.depacketizer.reset();
            self.damaged_timestamp = None;
            // The reorder buffer discarded whole stale timestamps. Resume at
            // the first packet of the intact newer burst instead of counting
            // that already-accounted gap a second time and discarding the
            // recovery frame as well.
            self.next_sequence = ready_packets.first().and_then(|packet| {
                RtpPacket::parse(packet)
                    .ok()
                    .map(|packet| packet.header.sequence)
            });
        }
        for packet in ready_packets {
            let packet = RtpPacket::parse(&packet)?;
            if self
                .next_sequence
                .is_some_and(|expected| expected != packet.header.sequence)
            {
                losses = losses.saturating_add(1);
                self.depacketizer.reset();
                self.damaged_timestamp = Some(packet.header.timestamp);
            }
            self.next_sequence = Some(packet.header.sequence.wrapping_add(1));
            if self.damaged_timestamp == Some(packet.header.timestamp) {
                continue;
            }
            self.damaged_timestamp = None;
            if let Some(unit) = self.depacketizer.push(&packet)? {
                let Some(position) = self
                    .first_packet_arrivals
                    .iter()
                    .position(|(timestamp, _)| *timestamp == unit.timestamp)
                else {
                    return Err(Error::Invalid(
                        "completed RTP access unit has no first-packet timestamp",
                    ));
                };
                let (_, first_packet_received_at) = self
                    .first_packet_arrivals
                    .remove(position)
                    .expect("timing tracker position was found");
                self.completed.push_back(TimedAccessUnit {
                    unit,
                    first_packet_received_at,
                    completed_at: Instant::now(),
                });
            }
        }
        Ok(PushOutcome {
            losses,
            dropped_timestamps,
        })
    }
}

/// Frame assembly across the streams that share one prediction chain.
pub struct VideoStreamAssembler {
    codec: MediaStreamCodec,
    /// Streams by SSRC, so both callers can work out which band a packet belongs
    /// to from the packet alone.
    streams: HashMap<u32, InboundStream>,
    order: Vec<u32>,
    batcher: AccessUnitBatcher,
    awaiting_sync: bool,
    pending_sync_followers: Vec<(usize, TimedAccessUnit)>,
}

impl VideoStreamAssembler {
    pub fn new(codec: MediaStreamCodec) -> Self {
        Self {
            codec,
            streams: HashMap::new(),
            order: Vec::new(),
            batcher: AccessUnitBatcher::default(),
            awaiting_sync: true,
            pending_sync_followers: Vec::with_capacity(AVC_VIDEO_SLICE_COUNT - 1),
        }
    }

    pub fn codec(&self) -> MediaStreamCodec {
        self.codec
    }

    /// The order the streams were first seen in. Band `n` is the `n`-th entry,
    /// which is how a batch's decode order maps onto a decoder's slice index.
    pub fn stream_order(&self) -> &[u32] {
        &self.order
    }

    /// Reserve the slot a stream's bands will occupy.
    ///
    /// A live receiver knows the four SSRCs from the negotiation, so it fixes
    /// the order before any packet arrives. A rebuild learns them from the
    /// packets themselves, in the order the dump has them.
    pub fn expect_stream(&mut self, ssrc: u32) {
        if self.order.contains(&ssrc) || self.order.len() >= AVC_VIDEO_SLICE_COUNT {
            return;
        }
        self.order.push(ssrc);
        self.streams
            .entry(ssrc)
            .or_insert_with(|| InboundStream::new(self.codec));
    }

    /// Feed one decrypted RTP packet, addressed by its SSRC.
    ///
    /// Packets from an unknown SSRC are ignored the way the live receiver
    /// ignores them: the server only uses the streams it negotiated, and a
    /// stray SSRC is not part of the chain.
    pub fn push_packet(
        &mut self,
        ssrc: u32,
        packet: &[u8],
        received_at: Instant,
    ) -> Result<PushOutcome> {
        // Resolve the slot before borrowing the stream, because the index is
        // what the chain orders bands by.
        let slot = match self.order.iter().position(|known| *known == ssrc) {
            Some(slot) => slot,
            None => {
                if self.order.len() >= AVC_VIDEO_SLICE_COUNT {
                    return Ok(PushOutcome::default());
                }
                self.order.push(ssrc);
                self.order.len() - 1
            }
        };
        if !self.streams.contains_key(&ssrc) {
            self.streams.insert(ssrc, InboundStream::new(self.codec));
        }
        let stream = self
            .streams
            .get_mut(&ssrc)
            .expect("the stream was just inserted");
        let outcome = stream.push(packet, received_at);
        // The slot is only used to keep the ordering deterministic; the batch
        // itself carries whichever bands completed.
        let _ = slot;
        outcome
    }

    /// Assemble the next decodable frame from what has arrived.
    ///
    /// This is the whole cross-stream state machine: collect every completed
    /// access unit, gate them on the sync state, insert them into the batcher,
    /// and release one frame at a time in the chain's own decode order.
    pub fn receive(&mut self) -> Result<AssembleOutcome> {
        let mut outcome = AssembleOutcome::default();
        let completed = self.take_completed();
        let mut late_units = 0_usize;
        let mut chain_failure: Option<&'static str> = None;

        'completed: for (slice_index, timed) in completed {
            if timed.unit.decode_order_number.is_none() {
                self.enter_sync_recovery();
                outcome.stream_error = true;
                return Err(Error::Invalid(
                    "native AVC access unit is missing its DON/DONL decode order",
                ));
            }
            let mut candidates = Vec::with_capacity(AVC_VIDEO_SLICE_COUNT);
            // Until a frame has been released, every IRAP is an origin
            // candidate. A usable origin has to be the start of a complete
            // four-band instant, and a stream that begins mid-instant — a raw
            // dump replayed from the middle of a session, or a receiver that
            // joins late — can never supply the units that precede what it saw.
            // Adopting the next IRAP instead of holding those units forever
            // costs nothing: an IRAP references no earlier picture, so no unit
            // that follows it depends on the ones dropped here.
            let needs_origin = self.awaiting_sync
                || (!self.batcher.released_any() && timed.unit.is_sync(self.codec));
            if needs_origin {
                if !timed.unit.is_sync(self.codec) {
                    self.hold_possible_sync_follower(slice_index, timed);
                    continue;
                }
                // The first IRAP/IDR after startup or a keyframe request is an
                // authoritative new decoding-order origin. UDP may complete a
                // following predictive band first, so retain only same-timestamp
                // DONs immediately following this sync unit.
                let decode_order = timed
                    .unit
                    .decode_order_number
                    .expect("missing DON/DONL was rejected above");
                let timestamp = timed.unit.timestamp;
                self.batcher.begin_prediction_chain(decode_order);
                self.awaiting_sync = false;
                candidates.push((slice_index, timed));
                candidates.extend(self.take_sync_followers(slice_index, timestamp, decode_order));
                candidates.sort_by_key(|(_, candidate)| {
                    candidate
                        .unit
                        .decode_order_number
                        .expect("sync followers carry DON/DONL")
                        .wrapping_sub(decode_order)
                });
            } else {
                candidates.push((slice_index, timed));
            }

            for (candidate_index, candidate) in candidates {
                match self.batcher.insert(candidate_index, candidate) {
                    BatchInsertResult::Accepted => {}
                    BatchInsertResult::IgnoredLate => {
                        late_units = late_units.saturating_add(1);
                    }
                    BatchInsertResult::MissingDecodeOrder => {
                        unreachable!("the assembler validates DON/DONL before inserting units")
                    }
                    BatchInsertResult::InvalidPredictionChain => {
                        chain_failure = Some("duplicate stream in one RTP timestamp");
                        break 'completed;
                    }
                    BatchInsertResult::PredictionChainOverflow => {
                        chain_failure = Some("pending DON/DONL sequence overflow");
                        break 'completed;
                    }
                }
            }
        }

        outcome.ignored_units = late_units;
        if let Some(reason) = chain_failure {
            if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                eprintln!("RTP cross-stream prediction chain reset ({reason})");
            }
            self.enter_sync_recovery();
            outcome.chain_reset = true;
        }

        outcome.frame = self.batcher.take_ready();
        Ok(outcome)
    }

    /// Throw away everything in flight and wait for the next sync frame.
    ///
    /// A live caller does this after a loss or when it has asked the server for a
    /// keyframe; a rebuild does it when the chain it is replaying was damaged.
    pub fn enter_sync_recovery(&mut self) {
        self.batcher.reset();
        self.awaiting_sync = true;
        self.pending_sync_followers.clear();
        for stream in self.streams.values_mut() {
            stream.reset_pending();
        }
    }

    /// Scan the streams in index order, collecting every completed access unit.
    fn take_completed(&mut self) -> VecDeque<(usize, TimedAccessUnit)> {
        let mut scheduled = Vec::new();
        for slot in 0..self.order.len() {
            let ssrc = self.order[slot];
            let Some(stream) = self.streams.get_mut(&ssrc) else {
                continue;
            };
            while let Some(unit) = stream.completed.pop_front() {
                insert_scheduled_item(&mut scheduled, slot, unit);
            }
        }
        scheduled.into()
    }

    fn hold_possible_sync_follower(&mut self, stream_index: usize, timed: TimedAccessUnit) {
        let timestamp = timed.unit.timestamp;
        if let Some(current_timestamp) = self
            .pending_sync_followers
            .first()
            .map(|(_, candidate)| candidate.unit.timestamp)
            && current_timestamp != timestamp
        {
            if timestamp_precedes(current_timestamp, timestamp) {
                self.pending_sync_followers.clear();
            } else {
                return;
            }
        }
        if self
            .pending_sync_followers
            .iter()
            .any(|(index, candidate)| {
                *index == stream_index
                    || candidate.unit.decode_order_number == timed.unit.decode_order_number
            })
        {
            return;
        }
        if self.pending_sync_followers.len() < AVC_VIDEO_SLICE_COUNT {
            self.pending_sync_followers.push((stream_index, timed));
        }
    }

    fn take_sync_followers(
        &mut self,
        sync_stream_index: usize,
        timestamp: u32,
        decode_order: u16,
    ) -> Vec<(usize, TimedAccessUnit)> {
        std::mem::take(&mut self.pending_sync_followers)
            .into_iter()
            .filter(|(stream_index, candidate)| {
                if *stream_index == sync_stream_index || candidate.unit.timestamp != timestamp {
                    return false;
                }
                candidate
                    .unit
                    .decode_order_number
                    .is_some_and(|candidate_order| {
                        let offset = candidate_order.wrapping_sub(decode_order);
                        (1..AVC_VIDEO_SLICE_COUNT as u16).contains(&offset)
                    })
            })
            .collect()
    }
}

/// Insert one completed access unit into the pending schedule, ordered by RTP
/// timestamp and then by stream, so equal timestamps keep stream order.
fn insert_scheduled_item(
    scheduled: &mut Vec<(usize, TimedAccessUnit)>,
    stream_index: usize,
    unit: TimedAccessUnit,
) {
    let position = scheduled
        .iter()
        .position(|(_, current)| timestamp_precedes(unit.unit.timestamp, current.unit.timestamp))
        .unwrap_or(scheduled.len());
    scheduled.insert(position, (stream_index, unit));
}

/// Whether `candidate` is a sampling instant that comes before `reference`,
/// with RTP timestamp wrap-around accounted for.
fn timestamp_precedes(candidate: u32, reference: u32) -> bool {
    (candidate.wrapping_sub(reference) as i32).is_negative()
}

/// Which depacketizer a codec needs.
enum VideoDepacketizer {
    H264(H264Depacketizer),
    Hevc(HevcDepacketizer),
}

impl VideoDepacketizer {
    fn new(codec: MediaStreamCodec) -> Self {
        match codec {
            MediaStreamCodec::H264 => Self::H264(H264Depacketizer::new()),
            MediaStreamCodec::Hevc => Self::Hevc(HevcDepacketizer::new_with_donl()),
        }
    }

    fn push(&mut self, packet: &RtpPacket<'_>) -> Result<Option<AccessUnit>> {
        match self {
            Self::H264(depacketizer) => depacketizer.push(packet),
            Self::Hevc(depacketizer) => depacketizer.push(packet),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::H264(depacketizer) => depacketizer.reset(),
            Self::Hevc(depacketizer) => depacketizer.reset(),
        }
    }
}

/// The horizontal bands sharing one RTP sampling instant. RFC 3550 defines the
/// RTP timestamp as the sampling instant; Apple's four adjacent SSRCs reuse it
/// for one serial prediction chain. Unchanged bands can be omitted, so
/// DON/DONL—not elapsed time or SSRC scan order—defines the batch boundary and
/// decoder submission order.
#[derive(Default)]
struct AccessUnitBatcher {
    pending: Vec<PendingFrameBatch>,
    initial_decode_order: Option<u16>,
    last_released_decode_order: Option<u16>,
}

struct PendingFrameBatch {
    timestamp: u32,
    first_packet_received_at: Instant,
    first_access_unit_completed_at: Instant,
    access_units: Vec<(usize, AccessUnit)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchInsertResult {
    Accepted,
    IgnoredLate,
    MissingDecodeOrder,
    InvalidPredictionChain,
    PredictionChainOverflow,
}

impl AccessUnitBatcher {
    /// Whether the chain has released at least one frame.
    ///
    /// Until it has, the assembler is still looking for an origin it can use.
    fn released_any(&self) -> bool {
        self.last_released_decode_order.is_some()
    }

    fn begin_prediction_chain(&mut self, decode_order: u16) {
        self.pending.clear();
        self.initial_decode_order = Some(decode_order);
        self.last_released_decode_order = None;
    }

    fn insert(&mut self, stream_index: usize, timed: TimedAccessUnit) -> BatchInsertResult {
        let TimedAccessUnit {
            unit,
            first_packet_received_at,
            completed_at,
        } = timed;
        let Some(decode_order) = unit.decode_order_number else {
            return BatchInsertResult::MissingDecodeOrder;
        };
        if self
            .last_released_decode_order
            .is_some_and(|released| !decode_order_is_newer(decode_order, released))
            || self.pending.iter().any(|batch| {
                batch
                    .access_units
                    .iter()
                    .any(|(_, pending)| pending.decode_order_number == Some(decode_order))
            })
        {
            return BatchInsertResult::IgnoredLate;
        }
        if let Some(batch) = self
            .pending
            .iter_mut()
            .find(|batch| batch.timestamp == unit.timestamp)
        {
            if batch
                .access_units
                .iter()
                .any(|(index, _)| *index == stream_index)
                || batch.access_units.len() >= AVC_VIDEO_SLICE_COUNT
            {
                return BatchInsertResult::InvalidPredictionChain;
            }
            batch.first_packet_received_at =
                batch.first_packet_received_at.min(first_packet_received_at);
            batch.first_access_unit_completed_at =
                batch.first_access_unit_completed_at.min(completed_at);
            batch.access_units.push((stream_index, unit));
            return BatchInsertResult::Accepted;
        }

        if self.pending.len() >= MAX_PENDING_FRAME_BATCHES {
            if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                let layout = self
                    .pending
                    .iter()
                    .map(|batch| {
                        (
                            batch.timestamp,
                            batch
                                .access_units
                                .iter()
                                .map(|(index, unit)| (*index, unit.decode_order_number))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>();
                eprintln!(
                    "RTP pending prediction chain overflow before stream={stream_index} timestamp={}: {layout:?}",
                    unit.timestamp,
                );
            }
            self.pending.clear();
            self.initial_decode_order = None;
            self.last_released_decode_order = None;
            return BatchInsertResult::PredictionChainOverflow;
        }
        let position = self
            .pending
            .iter()
            .position(|batch| timestamp_precedes(unit.timestamp, batch.timestamp))
            .unwrap_or(self.pending.len());
        self.pending.insert(
            position,
            PendingFrameBatch {
                timestamp: unit.timestamp,
                first_packet_received_at,
                first_access_unit_completed_at: completed_at,
                access_units: vec![(stream_index, unit)],
            },
        );
        BatchInsertResult::Accepted
    }

    /// Release the next decodable frame, if the chain proves one is complete.
    ///
    /// `released_at` only matters to a live caller, which reports how long a
    /// frame waited; assembly order never depends on wall-clock time, which is
    /// what lets a rebuild release the same frames in the same order.
    fn take_ready(&mut self) -> Option<AssembledFrame> {
        let (batch_index, first_decode_order) =
            if let Some(last_decode_order) = self.last_released_decode_order {
                let expected = last_decode_order.wrapping_add(1);
                let batch_index = self.pending.iter().position(|batch| {
                    batch
                        .access_units
                        .iter()
                        .any(|(_, unit)| unit.decode_order_number == Some(expected))
                })?;
                let batch = &self.pending[batch_index];
                let mut offsets = batch
                    .access_units
                    .iter()
                    .map(|(_, unit)| {
                        unit.decode_order_number
                            .expect("the batcher accepts only access units carrying DON/DONL")
                            .wrapping_sub(expected)
                    })
                    .collect::<Vec<_>>();
                offsets.sort_unstable();
                if !offsets
                    .iter()
                    .enumerate()
                    .all(|(index, offset)| usize::from(*offset) == index)
                {
                    return None;
                }
                let following_decode_order = expected.wrapping_add(batch.access_units.len() as u16);
                let boundary_proven = batch.access_units.len() == AVC_VIDEO_SLICE_COUNT
                    || self.pending.iter().enumerate().any(|(index, following)| {
                        index != batch_index
                            && following.access_units.iter().any(|(_, unit)| {
                                unit.decode_order_number == Some(following_decode_order)
                            })
                    });
                if !boundary_proven {
                    return None;
                }
                (batch_index, expected)
            } else {
                // A clean native AVC chain begins with one full four-band sync
                // timestamp. Do not infer an initial sparse boundary without a
                // preceding decoding-order number.
                let expected = self.initial_decode_order?;
                let batch_index = self.pending.iter().position(|batch| {
                    batch
                        .access_units
                        .iter()
                        .any(|(_, unit)| unit.decode_order_number == Some(expected))
                })?;
                let batch = &self.pending[batch_index];
                if batch.access_units.len() != AVC_VIDEO_SLICE_COUNT
                    || !decode_orders_are_contiguous(&batch.access_units, expected)
                {
                    return None;
                }
                (batch_index, expected)
            };

        let mut batch = self.pending.remove(batch_index);
        batch.access_units.sort_by_key(|(_, unit)| {
            unit.decode_order_number
                .expect("the batcher accepts only access units carrying DON/DONL")
                .wrapping_sub(first_decode_order)
        });
        self.last_released_decode_order = batch
            .access_units
            .last()
            .and_then(|(_, unit)| unit.decode_order_number);
        Some(AssembledFrame {
            timestamp: batch.timestamp,
            access_units: batch.access_units,
            first_packet_received_at: batch.first_packet_received_at,
            first_access_unit_completed_at: batch.first_access_unit_completed_at,
        })
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.initial_decode_order = None;
        self.last_released_decode_order = None;
    }
}

fn decode_orders_are_contiguous(access_units: &[(usize, AccessUnit)], start: u16) -> bool {
    (0..access_units.len()).all(|offset| {
        access_units
            .iter()
            .any(|(_, unit)| unit.decode_order_number == Some(start.wrapping_add(offset as u16)))
    })
}

fn decode_order_is_newer(candidate: u16, reference: u16) -> bool {
    candidate != reference && candidate.wrapping_sub(reference) < 0x8000
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        AVC_VIDEO_SLICE_COUNT, AccessUnitBatcher, BatchInsertResult, MAX_PENDING_FRAME_BATCHES,
        TimedAccessUnit, insert_scheduled_item,
    };
    use crate::media_stream::AccessUnit;

    fn unit(timestamp: u32, decode_order: u16) -> AccessUnit {
        AccessUnit {
            timestamp,
            decode_order_number: Some(decode_order),
            nal_units: vec![vec![decode_order as u8]],
        }
    }

    fn timed_unit(timestamp: u32, decode_order: u16, at: Instant) -> TimedAccessUnit {
        TimedAccessUnit {
            unit: unit(timestamp, decode_order),
            first_packet_received_at: at,
            completed_at: at,
        }
    }

    #[test]
    fn completed_slices_are_stably_sorted_in_stream_scan_order() {
        let now = Instant::now();
        let mut ready = Vec::new();
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            insert_scheduled_item(&mut ready, index, timed_unit(100, index as u16, now));
        }
        assert_eq!(
            ready
                .iter()
                .map(|(index, timed)| (*index, timed.unit.nal_units[0][0]))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 1), (2, 2), (3, 3)]
        );
    }

    #[test]
    fn sparse_updates_are_sorted_without_waiting_for_four_slices() {
        let now = Instant::now();
        let mut ready = Vec::new();
        insert_scheduled_item(&mut ready, 1, timed_unit(200, 1, now));
        insert_scheduled_item(&mut ready, 0, timed_unit(100, 0, now));
        insert_scheduled_item(&mut ready, 2, timed_unit(100, 2, now));
        assert_eq!(
            ready
                .iter()
                .map(|(index, timed)| (*index, timed.unit.timestamp))
                .collect::<Vec<_>>(),
            vec![(0, 100), (2, 100), (1, 200)]
        );
    }

    #[test]
    fn timestamp_sort_is_wrap_aware() {
        let now = Instant::now();
        let mut ready = Vec::new();
        insert_scheduled_item(&mut ready, 2, timed_unit(1, 2, now));
        insert_scheduled_item(&mut ready, 0, timed_unit(u32::MAX - 1, 0, now));
        insert_scheduled_item(&mut ready, 1, timed_unit(u32::MAX, 1, now));
        assert_eq!(
            ready
                .iter()
                .map(|(index, timed)| (*index, timed.unit.timestamp))
                .collect::<Vec<_>>(),
            vec![(0, u32::MAX - 1), (1, u32::MAX), (2, 1)]
        );
    }

    #[test]
    fn complete_four_slice_timestamp_is_released_immediately() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(100);
        for (slice, decode_order) in [(0, 102), (1, 100), (2, 103), (3, 101)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(800, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        let batch = batcher.take_ready().expect("complete timestamp");
        assert_eq!(batch.timestamp, 800);
        assert_eq!(batch.access_units.len(), AVC_VIDEO_SLICE_COUNT);
        assert_eq!(
            batch
                .access_units
                .iter()
                .map(|(slice, unit)| (*slice, unit.decode_order_number.unwrap()))
                .collect::<Vec<_>>(),
            vec![(1, 100), (3, 101), (0, 102), (2, 103)]
        );
    }

    #[test]
    fn batch_timing_separates_first_packet_from_first_completed_access_unit() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(20);
        for (slice, packet_ms, completed_ms) in [(0, 3, 7), (1, 1, 5), (2, 2, 4), (3, 4, 8)] {
            assert_eq!(
                batcher.insert(
                    slice,
                    TimedAccessUnit {
                        unit: unit(900, 20 + slice as u16),
                        first_packet_received_at: now + Duration::from_millis(packet_ms),
                        completed_at: now + Duration::from_millis(completed_ms),
                    },
                ),
                BatchInsertResult::Accepted
            );
        }
        let batch = batcher.take_ready().expect("complete timestamp");
        assert_eq!(
            batch.first_packet_received_at,
            now + Duration::from_millis(1)
        );
        assert_eq!(
            batch.first_access_unit_completed_at,
            now + Duration::from_millis(4)
        );
    }

    #[test]
    fn incomplete_timestamp_is_never_released_by_a_wall_clock_guess() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(40);
        assert_eq!(
            batcher.insert(2, timed_unit(1_600, 42, now)),
            BatchInsertResult::Accepted
        );
        assert!(batcher.take_ready().is_none());
        assert!(
            batcher.take_ready().is_none(),
            "elapsed time cannot prove a prediction subframe was omitted"
        );
        for (slice, decode_order) in [(0, 40), (1, 41), (3, 43)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(1_600, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        let batch = batcher.take_ready().expect("complete timestamp");
        assert_eq!(batch.timestamp, 1_600);
        assert_eq!(batch.access_units.len(), AVC_VIDEO_SLICE_COUNT);
    }

    /// Build one AVConference-style HEVC single-NAL packet: RTP header, the NAL
    /// header, the two-byte decoding-order number, then one payload byte.
    fn hevc_packet(
        sequence: u16,
        timestamp: u32,
        decode_order: u16,
        nal_type: u8,
        marker: bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0x80);
        out.push(96 | (u8::from(marker) * 0x80));
        out.extend_from_slice(&sequence.to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.push((nal_type << 1) & 0x7e);
        out.push(0x01);
        out.extend_from_slice(&decode_order.to_be_bytes());
        out.push(0xaa);
        out
    }

    fn push_synthetic(
        assembler: &mut super::VideoStreamAssembler,
        sequence: &mut u16,
        ssrc: u32,
        timestamp: u32,
        decode_order: u16,
        nal_type: u8,
    ) -> super::AssembleOutcome {
        // Every unit here is a whole access unit, so each carries the marker.
        let packet = hevc_packet(*sequence, timestamp, decode_order, nal_type, true);
        *sequence = sequence.wrapping_add(1);
        assembler
            .push_packet(ssrc, &packet, Instant::now())
            .expect("packet is accepted");
        assembler.receive().expect("assembly runs")
    }

    /// A stream whose first instant is torn can never complete that origin: the
    /// units before what the receiver saw are gone. Rather than holding them
    /// forever, the next keyframe becomes the origin — the IRAP references no
    /// earlier picture, so nothing that follows depends on the dropped units.
    ///
    /// This is the case a raw dump replayed from the middle of a session, or a
    /// receiver that joins late, runs into.
    #[test]
    fn an_unusable_origin_yields_to_the_next_keyframe() {
        let streams = [11_u32, 12, 13, 14];
        let mut sequences = [1_u16; 4];
        let mut assembler =
            super::VideoStreamAssembler::new(crate::media_stream::MediaStreamCodec::Hevc);
        for ssrc in streams {
            assembler.expect_stream(ssrc);
        }

        // Instant 1_000 carries the keyframe but is missing its fourth band.
        for (index, decode_order, nal_type) in [(0, 1_u16, 19_u8), (1, 2, 1), (2, 3, 1)] {
            let outcome = push_synthetic(
                &mut assembler,
                &mut sequences[index],
                streams[index],
                1_000,
                decode_order,
                nal_type,
            );
            assert!(outcome.frame.is_none(), "the torn origin must not release");
        }

        // A later keyframe provides a complete instant.
        let mut released = None;
        for (index, decode_order, nal_type) in [(0, 5_u16, 19_u8), (1, 6, 1), (2, 7, 1), (3, 8, 1)]
        {
            let outcome = push_synthetic(
                &mut assembler,
                &mut sequences[index],
                streams[index],
                2_000,
                decode_order,
                nal_type,
            );
            if let Some(frame) = outcome.frame {
                released = Some(frame);
            }
        }
        let frame = released.expect("the later keyframe becomes the origin");
        assert_eq!(frame.timestamp, 2_000);
        assert_eq!(frame.access_units.len(), AVC_VIDEO_SLICE_COUNT);
    }

    #[test]
    fn a_newer_full_timestamp_cannot_replace_an_incomplete_sync_origin() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(10);
        assert_eq!(
            batcher.insert(0, timed_unit(100, 10, now)),
            BatchInsertResult::Accepted
        );
        for (slice, decode_order) in [(0, 14), (1, 15), (2, 16), (3, 17)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(200, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        assert!(
            batcher.take_ready().is_none(),
            "decoder must not skip the sync timestamp's missing DONs"
        );
    }

    #[test]
    fn sparse_timestamp_releases_only_when_next_don_proves_its_boundary() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(0);
        for slice in 0..AVC_VIDEO_SLICE_COUNT {
            assert_eq!(
                batcher.insert(slice, timed_unit(100, slice as u16, now)),
                BatchInsertResult::Accepted
            );
        }
        batcher.take_ready().expect("initial sync batch");

        assert_eq!(
            batcher.insert(3, timed_unit(200, 5, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(0, timed_unit(200, 4, now)),
            BatchInsertResult::Accepted
        );
        assert!(batcher.take_ready().is_none());

        assert_eq!(
            batcher.insert(1, timed_unit(300, 6, now)),
            BatchInsertResult::Accepted
        );
        let sparse = batcher
            .take_ready()
            .expect("DON boundary proves sparse batch");
        assert_eq!(sparse.timestamp, 200);
        assert_eq!(
            sparse
                .access_units
                .iter()
                .map(|(slice, unit)| (*slice, unit.decode_order_number.unwrap()))
                .collect::<Vec<_>>(),
            vec![(0, 4), (3, 5)]
        );
    }

    #[test]
    fn a_decode_order_gap_blocks_newer_timestamps_until_the_missing_unit_arrives() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(0);
        for slice in 0..AVC_VIDEO_SLICE_COUNT {
            batcher.insert(slice, timed_unit(100, slice as u16, now));
        }
        batcher.take_ready().expect("initial sync batch");

        assert_eq!(
            batcher.insert(3, timed_unit(200, 5, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(0, timed_unit(300, 6, now)),
            BatchInsertResult::Accepted
        );
        assert!(batcher.take_ready().is_none(), "DON 4 is still missing");
        assert_eq!(
            batcher.insert(1, timed_unit(200, 4, now)),
            BatchInsertResult::Accepted
        );
        let recovered = batcher.take_ready().expect("contiguous DON chain");
        assert_eq!(recovered.timestamp, 200);
        assert_eq!(
            recovered
                .access_units
                .iter()
                .map(|(_, unit)| unit.decode_order_number.unwrap())
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    #[test]
    fn decode_order_wrap_is_contiguous_and_late_units_are_rejected() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(u16::MAX - 1);
        for (slice, decode_order) in [(0, u16::MAX), (1, 1), (2, u16::MAX - 1), (3, 0)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(u32::MAX, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        let first = batcher.take_ready().expect("wrapped initial batch");
        assert_eq!(
            first
                .access_units
                .iter()
                .map(|(_, unit)| unit.decode_order_number.unwrap())
                .collect::<Vec<_>>(),
            vec![u16::MAX - 1, u16::MAX, 0, 1]
        );
        assert_eq!(
            batcher.insert(3, timed_unit(1, u16::MAX, now)),
            BatchInsertResult::IgnoredLate
        );
        assert_eq!(
            batcher.insert(0, timed_unit(1, 2, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(1, timed_unit(2, 3, now)),
            BatchInsertResult::Accepted
        );
        let sparse = batcher.take_ready().expect("post-wrap sparse batch");
        assert_eq!(sparse.access_units[0].1.decode_order_number, Some(2));
    }

    #[test]
    fn missing_decode_order_and_duplicate_stream_are_explicit_errors() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        let mut missing = timed_unit(100, 0, now);
        missing.unit.decode_order_number = None;
        assert_eq!(
            batcher.insert(0, missing),
            BatchInsertResult::MissingDecodeOrder
        );
        assert_eq!(
            batcher.insert(0, timed_unit(100, 0, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(0, timed_unit(100, 1, now)),
            BatchInsertResult::InvalidPredictionChain
        );
    }

    #[test]
    fn incomplete_prediction_chain_is_bounded_and_reset() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        for frame in 0..MAX_PENDING_FRAME_BATCHES {
            assert_eq!(
                batcher.insert(0, timed_unit(100 + frame as u32, 10 + frame as u16, now),),
                BatchInsertResult::Accepted
            );
        }
        assert_eq!(
            batcher.insert(0, timed_unit(200, 30, now)),
            BatchInsertResult::PredictionChainOverflow
        );
        assert!(batcher.pending.is_empty());
        assert_eq!(batcher.last_released_decode_order, None);
    }
}
