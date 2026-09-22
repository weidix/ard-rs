//! MP4 container writer for recorded sessions.
//!
//! The recorder captures frames at the moments they are presented, which makes
//! the timeline variable-rate by construction: a static remote desktop produces
//! one long sample and a burst of changes produces many short ones. The writer
//! therefore stores the measured duration of every sample instead of pretending
//! to a fixed frame rate, so playback shows each frame for exactly as long as it
//! was on screen.
//!
//! Samples are written to the file as they are encoded (the container writer
//! flushes a chunk per second of timeline) and the index (`moov`) is appended on
//! finalize, so a recording that is still running is not yet playable.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use mp4::{MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use super::TIMESCALE;
use super::encoder::EncodedSample;

/// Everything the container needs about the encoded video except the samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VideoTrackFormat {
    pub width: u32,
    pub height: u32,
    /// H.264 sequence parameter set, without a start code.
    pub sps: Vec<u8>,
    /// H.264 picture parameter set, without a start code.
    pub pps: Vec<u8>,
}

/// Summary of one finished recording file.
#[derive(Debug, Clone)]
pub(crate) struct MuxerSummary {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_units: u64,
}

pub(crate) struct Mp4Muxer {
    writer: Option<Mp4Writer<BufWriter<File>>>,
    path: PathBuf,
    samples: u64,
    payload_bytes: u64,
    duration_units: u64,
}

impl Mp4Muxer {
    pub fn create(path: &Path) -> Result<Self, String> {
        let file = File::create(path).map_err(|error| format!("无法创建录制文件：{error}"))?;
        let config = Mp4Config {
            major_brand: "isom".parse().expect("valid brand"),
            minor_version: 512,
            compatible_brands: ["isom", "iso2", "avc1", "mp41"]
                .iter()
                .map(|brand| brand.parse().expect("valid brand"))
                .collect(),
            timescale: TIMESCALE,
        };
        let writer = Mp4Writer::write_start(BufWriter::new(file), &config)
            .map_err(|error| format!("无法写入 MP4 文件头：{error}"))?;
        Ok(Self {
            writer: Some(writer),
            path: path.to_path_buf(),
            samples: 0,
            payload_bytes: 0,
            duration_units: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    pub fn duration_units(&self) -> u64 {
        self.duration_units
    }

    /// The size of the file on disk, including everything flushed so far.
    pub fn file_bytes(&self) -> u64 {
        std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .unwrap_or(self.payload_bytes)
    }

    /// Append one encoded access unit, creating the video track on first use.
    pub fn write_sample(
        &mut self,
        format: &VideoTrackFormat,
        sample: EncodedSample,
    ) -> Result<(), String> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| "录制文件已经结束".to_owned())?;
        if self.samples == 0 {
            let width = u16::try_from(format.width)
                .map_err(|_| format!("录制宽度超出 MP4 限制：{}", format.width))?;
            let height = u16::try_from(format.height)
                .map_err(|_| format!("录制高度超出 MP4 限制：{}", format.height))?;
            let config = TrackConfig {
                track_type: TrackType::Video,
                timescale: TIMESCALE,
                language: "und".to_owned(),
                media_conf: MediaConfig::AvcConfig(mp4::AvcConfig {
                    width,
                    height,
                    seq_param_set: format.sps.clone(),
                    pic_param_set: format.pps.clone(),
                }),
            };
            writer
                .add_track(&config)
                .map_err(|error| format!("无法创建 MP4 视频轨道：{error}"))?;
        }
        // The container stores durations, so a zero-length sample would stall
        // playback at that frame and hide every later one.
        let duration = sample.duration.max(1);
        self.payload_bytes = self.payload_bytes.saturating_add(sample.bytes.len() as u64);
        self.duration_units = self.duration_units.saturating_add(u64::from(duration));
        writer
            .write_sample(
                1,
                &Mp4Sample {
                    start_time: sample.pts,
                    duration,
                    rendering_offset: 0,
                    is_sync: sample.is_sync,
                    bytes: sample.bytes.into(),
                },
            )
            .map_err(|error| format!("无法写入 MP4 样本：{error}"))?;
        self.samples += 1;
        Ok(())
    }

    /// Write the movie index and close the file.
    pub fn finalize(&mut self) -> Result<MuxerSummary, String> {
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| "录制文件已经结束".to_owned())?;
        writer
            .write_end()
            .map_err(|error| format!("无法写入 MP4 索引：{error}"))?;
        let mut file = writer.into_writer();
        file.flush()
            .map_err(|error| format!("无法刷新录制文件：{error}"))?;
        file.get_mut()
            .sync_all()
            .map_err(|error| format!("无法同步录制文件：{error}"))?;
        Ok(MuxerSummary {
            path: self.path.clone(),
            bytes: self.file_bytes(),
            duration_units: self.duration_units,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Mp4Muxer, VideoTrackFormat};
    use crate::recording::TIMESCALE;
    use crate::recording::encoder::EncodedSample;

    fn sample(pts: u64, duration: u32, is_sync: bool, payload: u8) -> EncodedSample {
        EncodedSample {
            bytes: vec![0, 0, 0, 2, 0x65, payload],
            is_sync,
            pts,
            duration,
        }
    }

    fn format() -> VideoTrackFormat {
        VideoTrackFormat {
            width: 640,
            height: 360,
            sps: vec![0x67, 0x64, 0x00, 0x1e],
            pps: vec![0x68, 0xee, 0x3c],
        }
    }

    #[test]
    fn written_file_is_a_parsable_mp4_with_variable_durations() {
        let directory = std::env::temp_dir().join(format!(
            "ard-recording-muxer-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");
        let path = directory.join("segment.mp4");
        let mut muxer = Mp4Muxer::create(&path).expect("muxer starts");
        let format = format();
        // 2 s at 25 fps, then a five-second static period, then one more frame.
        let frame = TIMESCALE / 25;
        muxer
            .write_sample(&format, sample(0, frame, true, 0x01))
            .expect("first sample");
        muxer
            .write_sample(
                &format,
                sample(u64::from(frame), 5 * TIMESCALE, false, 0x02),
            )
            .expect("second sample");
        muxer
            .write_sample(
                &format,
                sample(
                    u64::from(frame) + 5 * u64::from(TIMESCALE),
                    frame,
                    false,
                    0x03,
                ),
            )
            .expect("third sample");
        assert_eq!(muxer.samples(), 3);
        let summary = muxer.finalize().expect("finalized");
        assert_eq!(
            summary.duration_units,
            u64::from(frame) * 2 + 5 * u64::from(TIMESCALE)
        );
        assert!(summary.bytes > 0);

        let file = std::fs::File::open(&path).expect("recorded file opens");
        let size = file.metadata().expect("metadata").len();
        let mp4 = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size)
            .expect("recorded file is a valid mp4");
        let track = mp4.tracks().get(&1).expect("video track exists");
        assert_eq!(
            track.track_type().expect("video track"),
            mp4::TrackType::Video
        );
        assert_eq!(track.width(), 640);
        assert_eq!(track.height(), 360);
        assert_eq!(track.sample_count(), 3);
        assert_eq!(track.timescale(), TIMESCALE);
        assert_eq!(
            track.duration().as_millis(),
            (summary.duration_units as f64 / f64::from(TIMESCALE) * 1000.0).round() as u128
        );
        assert_eq!(
            track.sequence_parameter_set().expect("sps"),
            &[0x67, 0x64, 0x00, 0x1e]
        );
        assert_eq!(
            track.picture_parameter_set().expect("pps"),
            &[0x68, 0xee, 0x3c]
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_zero_length_sample_is_clamped_so_later_frames_stay_visible() {
        let directory = std::env::temp_dir().join(format!(
            "ard-recording-clamp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");
        let path = directory.join("segment.mp4");
        let mut muxer = Mp4Muxer::create(&path).expect("muxer starts");
        muxer
            .write_sample(&format(), sample(0, 0, true, 0x01))
            .expect("sample");
        let summary = muxer.finalize().expect("finalized");
        assert_eq!(summary.duration_units, 1);
        std::fs::remove_dir_all(&directory).ok();
    }
}
