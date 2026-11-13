//! `ffprobe` invocation and JSON parsing into the domain media model.
//!
//! The parser is deliberately tolerant: ffprobe omits fields freely (a stream
//! may have no `duration`, a container may have no `bit_rate`), and every
//! omitted field must degrade to `None` instead of failing the import.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::time::{FrameRate, Time};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Everything we know about one imported file.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub container: ContainerInfo,
    pub video: VideoStream,
    pub audio: Option<AudioStream>,
    /// Other streams we keep for display but do not edit (subtitles, data).
    pub extra_streams: Vec<ExtraStream>,
    /// Container duration; the authoritative timeline length.
    pub duration: Time,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerInfo {
    /// First entry of ffprobe's `format_name`, e.g. `mov` for `mov,mp4,m4a,...`.
    pub format_name: String,
    pub format_long_name: Option<String>,
    pub bit_rate: Option<u64>,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoStream {
    pub index: u32,
    pub codec: String,
    pub codec_long_name: Option<String>,
    pub profile: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Coded size, which can exceed the display size for cropped H.264.
    pub coded_width: Option<u32>,
    pub coded_height: Option<u32>,
    pub pix_fmt: Option<String>,
    /// Frames per second as reported by the container (`avg_frame_rate`),
    /// falling back to `r_frame_rate` when the average is 0/0.
    pub frame_rate: FrameRate,
    /// Rotation in degrees to apply on display, normalized to 0/90/180/270.
    pub rotation: i32,
    pub bit_rate: Option<u64>,
    pub nb_frames: Option<u64>,
    pub sample_aspect_ratio: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub codec_long_name: Option<String>,
    pub sample_rate: u32,
    pub channels: u32,
    pub channel_layout: Option<String>,
    pub bit_rate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraStream {
    pub index: u32,
    pub codec_type: String,
    pub codec: String,
}

impl MediaInfo {
    /// Display size after applying rotation metadata.
    pub fn display_size(&self) -> (u32, u32) {
        match self.video.rotation.rem_euclid(180) {
            90 => (self.video.height, self.video.width),
            _ => (self.video.width, self.video.height),
        }
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }
}

/// Runs `ffprobe` and parses the result.
pub fn probe(install: &FfmpegInstall, path: &Path) -> Result<MediaInfo, CoreError> {
    if !path.is_file() {
        return Err(CoreError::io(
            path,
            std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
        ));
    }

    let output = hidden_command(&install.ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: install.ffprobe.clone(),
            source,
        })?;

    if !output.status.success() {
        return Err(CoreError::FfmpegFailed {
            program: install.ffprobe.display().to_string(),
            code: output.status.code(),
            stderr: summarize_stderr(&output.stderr),
        });
    }

    parse(path, &output.stdout)
}

/// Parses already-captured ffprobe JSON. Split out so tests can feed fixtures.
pub fn parse(path: &Path, json: &[u8]) -> Result<MediaInfo, CoreError> {
    let raw: RawProbe = serde_json::from_slice(json).map_err(|err| CoreError::FfmpegBadOutput {
        program: "ffprobe".to_owned(),
        detail: err.to_string(),
    })?;

    let video = raw
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| CoreError::NoVideoStream {
            path: path.to_path_buf(),
        })?;

    let audio = raw
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));

    let extra_streams = raw
        .streams
        .iter()
        .filter(|stream| {
            !matches!(
                stream.codec_type.as_deref(),
                Some("video") | Some("audio") | None
            )
        })
        .map(|stream| ExtraStream {
            index: stream.index.unwrap_or(0),
            codec_type: stream.codec_type.clone().unwrap_or_default(),
            codec: stream.codec_name.clone().unwrap_or_default(),
        })
        .collect();

    // Prefer the container duration; fall back to the video stream's own
    // duration for formats that only record it per stream.
    let duration = raw
        .format
        .as_ref()
        .and_then(|format| format.duration.as_deref())
        .and_then(Time::parse_secs)
        .or_else(|| video.duration.as_deref().and_then(Time::parse_secs))
        .ok_or_else(|| CoreError::UnknownDuration {
            path: path.to_path_buf(),
        })?;

    let format = raw.format.unwrap_or_default();

    Ok(MediaInfo {
        path: path.to_path_buf(),
        container: ContainerInfo {
            format_name: format
                .format_name
                .as_deref()
                .and_then(|name| name.split(',').next())
                .unwrap_or("unknown")
                .to_owned(),
            format_long_name: format.format_long_name,
            bit_rate: format.bit_rate.and_then(|v| v.parse().ok()),
            size: format.size.and_then(|v| v.parse().ok()),
        },
        video: parse_video(video),
        audio: audio.map(parse_audio),
        extra_streams,
        duration,
    })
}

fn parse_video(stream: &RawStream) -> VideoStream {
    let frame_rate = stream
        .avg_frame_rate
        .as_deref()
        .and_then(FrameRate::parse)
        .filter(|rate| rate.fps() > 0.5)
        .or_else(|| stream.r_frame_rate.as_deref().and_then(FrameRate::parse))
        .unwrap_or_default();

    VideoStream {
        index: stream.index.unwrap_or(0),
        codec: stream.codec_name.clone().unwrap_or_default(),
        codec_long_name: stream.codec_long_name.clone(),
        profile: stream.profile.clone(),
        width: stream.width.unwrap_or(0),
        height: stream.height.unwrap_or(0),
        coded_width: stream.coded_width,
        coded_height: stream.coded_height,
        pix_fmt: stream.pix_fmt.clone(),
        frame_rate,
        rotation: stream.rotation(),
        bit_rate: stream.bit_rate.as_deref().and_then(|v| v.parse().ok()),
        nb_frames: stream.nb_frames.as_deref().and_then(|v| v.parse().ok()),
        sample_aspect_ratio: stream.sample_aspect_ratio.clone(),
    }
}

fn parse_audio(stream: &RawStream) -> AudioStream {
    AudioStream {
        index: stream.index.unwrap_or(0),
        codec: stream.codec_name.clone().unwrap_or_default(),
        codec_long_name: stream.codec_long_name.clone(),
        sample_rate: stream
            .sample_rate
            .as_deref()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        channels: stream.channels.unwrap_or(0),
        channel_layout: stream.channel_layout.clone(),
        bit_rate: stream.bit_rate.as_deref().and_then(|v| v.parse().ok()),
    }
}

/// Keeps only the last few stderr lines; ffprobe failures are usually verbose
/// and the UI only has room for a summary.
fn summarize_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(4);
    lines[start..].join("\n")
}

// --- Raw ffprobe schema ---------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    format_long_name: Option<String>,
    /// ffprobe emits numbers as strings.
    bit_rate: Option<String>,
    size: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    index: Option<u32>,
    codec_name: Option<String>,
    codec_long_name: Option<String>,
    codec_type: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    coded_width: Option<u32>,
    coded_height: Option<u32>,
    pix_fmt: Option<String>,
    sample_aspect_ratio: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    nb_frames: Option<String>,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
    /// Some muxers put rotation in the stream tags instead.
    #[serde(default)]
    tags: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawSideData {
    rotation: Option<f64>,
}

impl RawStream {
    /// Rotation from side data (`displaymatrix`), falling back to the
    /// `rotate` tag older muxers wrote. Normalized to 0/90/180/270.
    fn rotation(&self) -> i32 {
        let from_side_data = self
            .side_data_list
            .iter()
            .find_map(|side| side.rotation)
            .map(|deg| deg.round() as i32);

        let raw = from_side_data.or_else(|| {
            self.tags
                .get("rotate")
                .and_then(|value| value.parse::<f64>().ok())
                .map(|deg| deg.round() as i32)
        });

        match raw {
            Some(deg) => deg.rem_euclid(360),
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/ffprobe")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
    }

    #[test]
    fn parses_mp4_fixture() {
        let info = parse(Path::new("sample_h264.mp4"), &fixture("sample_h264.json")).unwrap();

        assert_eq!(info.container.format_name, "mov");
        assert_eq!(info.duration, Time::from_secs(4.0));

        assert_eq!(info.video.codec, "h264");
        assert_eq!(info.video.profile.as_deref(), Some("High"));
        assert_eq!((info.video.width, info.video.height), (1280, 720));
        assert_eq!(info.video.pix_fmt.as_deref(), Some("yuv420p"));
        assert_eq!(info.video.frame_rate, FrameRate { num: 30, den: 1 });
        assert_eq!(info.video.nb_frames, Some(120));
        assert_eq!(info.video.rotation, 0);

        let audio = info.audio.expect("audio stream");
        assert_eq!(audio.codec, "aac");
        assert_eq!(audio.sample_rate, 44100);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.channel_layout.as_deref(), Some("mono"));

        assert!(info.extra_streams.is_empty());
    }

    #[test]
    fn parses_webm_without_audio() {
        let info = parse(
            Path::new("sample_noaudio.webm"),
            &fixture("sample_noaudio.json"),
        )
        .unwrap();
        assert_eq!(info.container.format_name, "matroska");
        assert_eq!(info.duration, Time::from_secs(3.0));
        assert_eq!(info.video.codec, "vp9");
        assert_eq!((info.video.width, info.video.height), (640, 360));
        assert!(info.audio.is_none(), "webm fixture has no audio stream");
        assert!(!info.has_audio());
    }

    #[test]
    fn parses_real_rotated_file() {
        let info = parse(
            Path::new("sample_rotated.mp4"),
            &fixture("sample_rotated.json"),
        )
        .unwrap();
        // Stored as landscape, meant to be shown portrait.
        assert_eq!((info.video.width, info.video.height), (1280, 720));
        assert_eq!(info.video.rotation, 90);
        assert_eq!(info.display_size(), (720, 1280));
    }

    #[test]
    fn derives_display_size_from_rotation() {
        let mut info = parse(Path::new("x.mp4"), &fixture("sample_h264.json")).unwrap();
        assert_eq!(info.display_size(), (1280, 720));

        info.video.rotation = 90;
        assert_eq!(info.display_size(), (720, 1280));

        info.video.rotation = 270;
        assert_eq!(info.display_size(), (720, 1280));

        info.video.rotation = 180;
        assert_eq!(info.display_size(), (1280, 720));
    }

    #[test]
    fn missing_video_stream_is_an_error() {
        let json = br#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"mp3"}]}"#;
        let err = parse(Path::new("audio-only.mp3"), json).unwrap_err();
        assert!(matches!(err, CoreError::NoVideoStream { .. }));
    }

    #[test]
    fn missing_duration_is_an_error() {
        let json = br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":320,"height":240}]}"#;
        let err = parse(Path::new("x.mp4"), json).unwrap_err();
        assert!(matches!(err, CoreError::UnknownDuration { .. }));
    }

    #[test]
    fn falls_back_to_stream_duration() {
        let json = br#"{
            "streams": [{
                "index": 0, "codec_type": "video", "codec_name": "h264",
                "width": 320, "height": 240, "duration": "12.500000",
                "avg_frame_rate": "25/1"
            }]
        }"#;
        let info = parse(Path::new("x.mp4"), json).unwrap();
        assert_eq!(info.duration, Time(12_500_000));
        assert_eq!(info.video.frame_rate, FrameRate { num: 25, den: 1 });
    }

    #[test]
    fn accepts_variable_frame_rate_metadata() {
        // VFR files report avg_frame_rate 0/0; the parser must fall back to
        // r_frame_rate instead of producing a zero-division FrameRate.
        let json = br#"{
            "streams": [{
                "index": 0, "codec_type": "video", "codec_name": "hevc",
                "width": 1920, "height": 1080, "duration": "1.0",
                "avg_frame_rate": "0/0", "r_frame_rate": "24000/1001"
            }]
        }"#;
        let info = parse(Path::new("vfr.mkv"), json).unwrap();
        assert_eq!(
            info.video.frame_rate,
            FrameRate {
                num: 24000,
                den: 1001
            }
        );
    }

    #[test]
    fn reads_rotation_from_side_data() {
        let json = br#"{
            "format": {"format_name": "mov,mp4", "duration": "1.0"},
            "streams": [{
                "index": 0, "codec_type": "video", "codec_name": "h264",
                "width": 1920, "height": 1080,
                "side_data_list": [{"rotation": -90}]
            }]
        }"#;
        let info = parse(Path::new("rotated.mp4"), json).unwrap();
        assert_eq!(info.video.rotation, 270);
    }

    #[test]
    fn reads_rotation_from_legacy_tag() {
        let json = br#"{
            "format": {"format_name": "mov", "duration": "1.0"},
            "streams": [{
                "index": 0, "codec_type": "video", "codec_name": "h264",
                "width": 1920, "height": 1080,
                "tags": {"rotate": "90"}
            }]
        }"#;
        let info = parse(Path::new("rotated.mp4"), json).unwrap();
        assert_eq!(info.video.rotation, 90);
    }

    #[test]
    fn keeps_non_av_streams_as_extras() {
        let json = br#"{
            "format": {"format_name": "matroska,webm", "duration": "1.0"},
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "vp9", "width": 640, "height": 360},
                {"index": 1, "codec_type": "subtitle", "codec_name": "ass"},
                {"index": 2, "codec_type": "data", "codec_name": "bin_data"}
            ]
        }"#;
        let info = parse(Path::new("x.mkv"), json).unwrap();
        assert_eq!(info.extra_streams.len(), 2);
        assert_eq!(info.extra_streams[0].codec_type, "subtitle");
        assert_eq!(info.extra_streams[1].codec, "bin_data");
        assert!(info.audio.is_none());
    }

    #[test]
    fn malformed_json_is_reported_as_bad_output() {
        let err = parse(Path::new("x.mp4"), b"{not json").unwrap_err();
        assert!(matches!(err, CoreError::FfmpegBadOutput { .. }));
    }
}
