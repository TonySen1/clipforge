//! Precise export: re-encode the timeline through a filter graph.
//!
//! Lossless cuts snap to keyframes and cannot represent gaps; re-encoding is
//! the escape hatch for both. The cost is generation loss and time, so the mode
//! is opt-in and the dialog says so.
//!
//! The graph is built as one `concat` filter over per-segment `trim`+`setpts`
//! chains. Everything is normalised to the first segment's geometry so mixed
//! sources still concatenate.

use super::{has_gaps, only_segments, ExportBlocker, Segment, TimelinePiece};
use crate::domain::Project;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::media::probe::MediaInfo;
use crate::time::Time;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which video encoder to use.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum VideoCodecChoice {
    #[default]
    H264,
    H265,
    Vp9,
}

impl VideoCodecChoice {
    /// Encoder name, or `None` when the build does not provide it.
    pub fn encoder(self, install: &FfmpegInstall) -> Option<&'static str> {
        let encoders = &install.version.encoders;
        match self {
            Self::H264 if encoders.libx264 => Some("libx264"),
            Self::H265 if encoders.libx265 => Some("libx265"),
            Self::Vp9 if encoders.libvpx_vp9 => Some("libvpx-vp9"),
            // Hardware-independent fallbacks that ship with every build.
            Self::H264 => Some("mpeg4"),
            Self::H265 => Some("libx265"),
            Self::Vp9 => None,
        }
    }

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::H264 => "export.codec.h264",
            Self::H265 => "export.codec.h265",
            Self::Vp9 => "export.codec.vp9",
        }
    }

    /// Picks the codec a container can hold.
    pub fn for_container(container: &str, install: &FfmpegInstall) -> Self {
        match container {
            "webm" => Self::Vp9,
            _ => {
                if install.version.encoders.libx264 {
                    Self::H264
                } else if install.version.encoders.libx265 {
                    Self::H265
                } else {
                    Self::H264
                }
            }
        }
    }
}

/// Encoding parameters for a precise export.
#[derive(Clone, Debug, PartialEq)]
pub struct PreciseSettings {
    pub codec: VideoCodecChoice,
    pub crf: u32,
    /// `None` reproduces the source's rate; `Some(rate)` forces a constant rate.
    pub frame_rate: Option<crate::FrameRate>,
    /// Output size. Defaults to the first segment's display size.
    pub size: Option<(u32, u32)>,
    pub audio_bitrate_kbps: u32,
    /// Fill gaps with black/silence rather than refusing.
    pub fill_gaps: bool,
}

impl Default for PreciseSettings {
    fn default() -> Self {
        Self {
            codec: VideoCodecChoice::H264,
            crf: 18,
            frame_rate: None,
            size: None,
            audio_bitrate_kbps: 192,
            fill_gaps: true,
        }
    }
}

impl PreciseSettings {
    /// Settings for a container, choosing the codec it can actually hold.
    pub fn for_container(container: &str, install: &FfmpegInstall) -> Self {
        Self {
            codec: VideoCodecChoice::for_container(container, install),
            ..Self::default()
        }
    }
}

/// A validated precise-export job.
#[derive(Clone, Debug, PartialEq)]
pub struct PrecisePlan {
    /// Timeline pieces in order; gaps are explicit and will be filled.
    pub pieces: Vec<TimelinePiece>,
    /// Source ids that actually carry an audio stream. A source without one
    /// must get synthesised silence in the graph, not an `[i:a]` link that
    /// does not exist.
    pub sources_with_audio: Vec<crate::SourceId>,
    /// Total output duration, gaps included.
    pub duration: Time,
    /// Geometry every segment is scaled to.
    pub width: u32,
    pub height: u32,
    /// Frame rate the output is resampled to.
    pub frame_rate: crate::FrameRate,
    pub settings: PreciseSettings,
    pub blocker: Option<ExportBlocker>,
}

impl PrecisePlan {
    pub fn is_runnable(&self) -> bool {
        self.blocker.is_none()
    }

    pub fn has_gaps(&self) -> bool {
        has_gaps(&self.pieces)
    }

    /// Real segments only, in export order.
    pub fn segments(&self) -> Vec<Segment> {
        only_segments(&self.pieces)
    }

    pub fn has_audio(&self, id: crate::SourceId) -> bool {
        self.sources_with_audio.contains(&id)
    }
}

/// Builds and validates a precise plan for a timeline window.
pub fn plan_precise(
    project: &Project,
    start: Time,
    end: Time,
    settings: PreciseSettings,
) -> PrecisePlan {
    let pieces = match super::timeline_pieces(project, start, end) {
        Ok(pieces) => pieces,
        Err(blocker) => {
            return PrecisePlan {
                pieces: Vec::new(),
                sources_with_audio: Vec::new(),
                duration: Time::ZERO,
                width: 0,
                height: 0,
                frame_rate: crate::FrameRate::FALLBACK,
                settings,
                blocker: Some(blocker),
            };
        }
    };

    // Geometry and rate come from the first real segment so the output looks
    // like the material the user started with.
    let reference = only_segments(&pieces)
        .first()
        .and_then(|segment| project.source(segment.source_id))
        .map(|source| source.info.as_ref());

    let (width, height) = settings
        .size
        .or_else(|| reference.map(|info| info.display_size()))
        .unwrap_or((1280, 720));

    let frame_rate = settings
        .frame_rate
        .or_else(|| reference.map(|info| info.video.frame_rate))
        .unwrap_or(crate::FrameRate::FALLBACK);

    let duration = end.saturating_sub(start);

    let mut blocker = None;
    if only_segments(&pieces).is_empty() {
        // A window entirely inside a gap has nothing to encode.
        blocker = Some(ExportBlocker::NothingToExport);
    } else if width == 0 || height == 0 {
        blocker = Some(ExportBlocker::NothingToExport);
    } else if has_gaps(&pieces) && !settings.fill_gaps {
        blocker = Some(ExportBlocker::TimelineHasGaps);
    }

    let sources_with_audio = {
        let mut ids = Vec::new();
        for piece in &pieces {
            if let TimelinePiece::Clip(segment) = piece {
                if ids.contains(&segment.source_id) {
                    continue;
                }
                if project
                    .source(segment.source_id)
                    .map(|source| source.has_audio())
                    .unwrap_or(false)
                {
                    ids.push(segment.source_id);
                }
            }
        }
        ids
    };

    PrecisePlan {
        pieces,
        sources_with_audio,
        duration,
        width,
        height,
        frame_rate,
        settings,
        blocker,
    }
}

/// Builds the `-filter_complex` graph for a plan.
///
/// Each segment becomes
/// `[i:v] trim,setpts,scale,pad [vN]` and `[i:a] atrim,asetpts,aresample [aN]`,
/// then one `concat` joins them. Gaps are synthesised from `color` and
/// `anullsrc` so the output has no timestamp holes.
pub fn build_filter_graph(plan: &PrecisePlan) -> String {
    let mut chains: Vec<String> = Vec::new();
    let mut video_labels: Vec<String> = Vec::new();
    let mut audio_labels: Vec<String> = Vec::new();

    let mut input_index = 0usize;

    for (position, piece) in plan.pieces.iter().enumerate() {
        match piece {
            TimelinePiece::Clip(segment) => {
                let duration = segment.duration().as_secs_f64();
                if duration <= 0.0 {
                    continue;
                }

                let v = format!("v{position}");
                let a = format!("a{position}");

                chains.push(format!(
                    "[{input_index}:v]trim=start={start:.6}:duration={dur:.6},setpts=PTS-STARTPTS,\
scale={w}:{h}:force_original_aspect_ratio=decrease,\
pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}[{v}]",
                    input_index = input_index,
                    start = segment.source_in.as_secs_f64(),
                    dur = duration,
                    w = plan.width,
                    h = plan.height,
                    fps = plan.frame_rate.fps(),
                    v = v
                ));
                video_labels.push(v);

                // A segment whose source has no audio stream cannot link
                // `[i:a]`; synthesise silence of the same length instead.
                if plan.has_audio(segment.source_id) {
                    chains.push(format!(
                        "[{input_index}:a]atrim=start={start:.6}:duration={dur:.6},asetpts=PTS-STARTPTS,\
aresample=async=1:first_pts=0[{a}]",
                        input_index = input_index,
                        start = segment.source_in.as_secs_f64(),
                        dur = duration,
                        a = a
                    ));
                } else {
                    chains.push(format!(
                        "anullsrc=r={rate}:cl=stereo,atrim=duration={dur:.6},asetpts=PTS-STARTPTS[{a}]",
                        rate = 48_000,
                        dur = duration,
                        a = a
                    ));
                }
                audio_labels.push(a);

                input_index += 1;
            }
            TimelinePiece::Gap { start, end } => {
                // A gap has no input; synthesise it from lavfi sources.
                let duration = end.saturating_sub(*start).as_secs_f64();
                if duration <= 0.0 {
                    continue;
                }

                let v = format!("v{position}");
                let a = format!("a{position}");
                chains.push(format!(
                    "color=c=black:s={w}x{h}:r={fps}:d={dur:.6},setsar=1[{v}]",
                    w = plan.width,
                    h = plan.height,
                    fps = plan.frame_rate.fps(),
                    dur = duration,
                    v = v
                ));
                chains.push(format!(
                    "anullsrc=r={rate}:cl=stereo,atrim=duration={dur:.6}[{a}]",
                    rate = 48_000,
                    dur = duration,
                    a = a
                ));
                video_labels.push(v);
                audio_labels.push(a);
            }
        }
    }

    let inputs: String = video_labels
        .iter()
        .zip(audio_labels.iter())
        .map(|(v, a)| format!("[{v}][{a}]"))
        .collect();

    let concat = format!(
        "{inputs}concat=n={n}:v=1:a=1[vout][aout]",
        inputs = inputs,
        n = video_labels.len()
    );

    chains.push(concat);
    chains.join(";")
}

/// Builds the full `ffmpeg` command for a precise export.
///
/// `segments` is passed separately from the plan so the caller can supply the
/// real input paths in export order.
pub fn build_precise_command(
    install: &FfmpegInstall,
    plan: &PrecisePlan,
    segment_inputs: &[PathBuf],
    temp_output: &Path,
    output_for_container: &Path,
    has_audio_anywhere: bool,
) -> Result<Command, ExportBlocker> {
    let encoder = plan
        .settings
        .codec
        .encoder(install)
        .ok_or_else(|| ExportBlocker::UnsupportedContainer("encoder unavailable".to_owned()))?;

    let container = super::container_for(output_for_container)
        .ok_or_else(|| ExportBlocker::UnsupportedContainer("unknown container".to_owned()))?;

    let mut cmd = hidden_command(&install.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y"]);

    for path in segment_inputs {
        cmd.arg("-i").arg(path);
    }

    // Gap synthesis needs a silent-audio template if any segment lacks audio.
    let needs_silence = plan.has_gaps() || !has_audio_anywhere;
    if needs_silence {
        cmd.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]);
    }

    let graph = build_filter_graph(plan);
    cmd.args(["-filter_complex", &graph]);
    cmd.args(["-map", "[vout]", "-map", "[aout]"]);

    // Video encoding.
    cmd.args(["-c:v", encoder]);
    match plan.settings.codec {
        VideoCodecChoice::Vp9 => {
            cmd.args(["-crf", &plan.settings.crf.to_string()]);
            cmd.args(["-b:v", "0"]);
            cmd.args(["-row-mt", "1"]);
        }
        VideoCodecChoice::H265 => {
            cmd.args(["-crf", &plan.settings.crf.to_string()]);
            cmd.args(["-preset", "medium"]);
            // libx265 writes its banner to stderr, which pollutes the log tail.
            cmd.args(["-x265-params", "log-level=error"]);
        }
        VideoCodecChoice::H264 => {
            cmd.args(["-crf", &plan.settings.crf.to_string()]);
            cmd.args(["-preset", "medium"]);
            cmd.args(["-pix_fmt", "yuv420p"]);
        }
    }

    // Audio encoding: match the container.
    match container {
        "webm" => {
            if install.version.encoders.libopus {
                cmd.args(["-c:a", "libopus"]);
            } else {
                cmd.args(["-c:a", "libvorbis"]);
            }
        }
        _ => {
            cmd.args(["-c:a", "aac"]);
            cmd.args(["-b:a", &format!("{}k", plan.settings.audio_bitrate_kbps)]);
        }
    }

    cmd.args(["-progress", "pipe:1", "-nostats"]);
    cmd.args(["-f", container]);
    cmd.arg(temp_output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    Ok(cmd)
}

/// Reference MediaInfo for a plan, used by callers that need the geometry.
pub fn reference_info<'a>(project: &'a Project, plan: &PrecisePlan) -> Option<&'a MediaInfo> {
    plan.segments()
        .first()
        .and_then(|segment| project.source(segment.source_id))
        .map(|source| source.info.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MediaSource;
    use crate::media::{probe, KeyframeIndex};

    fn info(name: &str, seconds: f64) -> MediaInfo {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        let mut media = probe::parse(std::path::Path::new(name), &json).unwrap();
        media.duration = Time::from_secs(seconds);
        media
    }

    fn project(lengths: &[f64], gaps: &[f64]) -> Project {
        let mut project = Project::empty();
        for (index, len) in lengths.iter().enumerate() {
            let id = project.alloc_source_id();
            let mut media = info(&format!("a{index}.mp4"), *len);
            let mut source = MediaSource::new(
                id,
                media.clone(),
                KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(*len)]),
            );
            source.id = id;
            media.duration = Time::from_secs(*len);
            let clip_id = project.add_source(source);
            if let Some(gap) = gaps.get(index) {
                if *gap > 0.0 {
                    project.move_clip(clip_id, Time::from_secs(*gap)).unwrap();
                }
            }
        }
        project
    }

    fn install_with(encoders: crate::media::EncoderSupport) -> FfmpegInstall {
        FfmpegInstall {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            version: crate::media::FfmpegVersion {
                banner: String::new(),
                major: Some(7),
                git_build: false,
                gpl: true,
                encoders,
            },
        }
    }

    fn full_encoders() -> crate::media::EncoderSupport {
        crate::media::EncoderSupport {
            libx264: true,
            libx265: true,
            libvpx_vp9: true,
            aac: true,
            libopus: true,
        }
    }

    #[test]
    fn plan_uses_the_first_segment_geometry() {
        let project = project(&[4.0, 3.0], &[]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(7.0),
            PreciseSettings::default(),
        );

        assert!(plan.is_runnable());
        assert_eq!((plan.width, plan.height), (1280, 720));
        assert_eq!(plan.duration, Time::from_secs(7.0));
        assert!(!plan.has_gaps());
    }

    #[test]
    fn plan_refuses_gaps_when_filling_is_off() {
        let project = project(&[3.0, 3.0], &[0.0, 5.0]);
        let settings = PreciseSettings {
            fill_gaps: false,
            ..Default::default()
        };
        let plan = plan_precise(&project, Time::ZERO, Time::from_secs(8.0), settings);

        assert_eq!(plan.blocker, Some(ExportBlocker::TimelineHasGaps));
        assert!(!plan.is_runnable());
    }

    #[test]
    fn plan_accepts_gaps_when_filling_is_on() {
        let project = project(&[3.0, 3.0], &[0.0, 5.0]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(8.0),
            PreciseSettings::default(),
        );
        assert!(plan.is_runnable());
        assert!(plan.has_gaps());
    }

    #[test]
    fn filter_graph_trims_and_normalises_each_segment() {
        let project = project(&[4.0, 3.0], &[]);
        let plan = plan_precise(
            &project,
            Time::from_secs(1.0),
            Time::from_secs(6.0),
            PreciseSettings::default(),
        );
        let graph = build_filter_graph(&plan);

        assert!(graph.contains("trim=start="), "{graph}");
        assert!(graph.contains("setpts=PTS-STARTPTS"), "{graph}");
        assert!(graph.contains("scale=1280:720"), "{graph}");
        assert!(graph.contains("pad=1280:720"), "{graph}");
        assert!(graph.contains("setsar=1"), "{graph}");
        assert!(graph.contains("atrim=start="), "{graph}");
        assert!(graph.contains("concat=n=2:v=1:a=1[vout][aout]"), "{graph}");
    }

    #[test]
    fn filter_graph_fills_gaps_with_black_and_silence() {
        let project = project(&[3.0, 3.0], &[0.0, 5.0]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(8.0),
            PreciseSettings::default(),
        );
        let graph = build_filter_graph(&plan);

        assert!(graph.contains("color=c=black"), "{graph}");
        assert!(graph.contains("anullsrc"), "{graph}");
    }

    #[test]
    fn filter_graph_handles_a_single_segment() {
        let project = project(&[4.0], &[]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(4.0),
            PreciseSettings::default(),
        );
        let graph = build_filter_graph(&plan);
        assert!(graph.contains("concat=n=1"), "{graph}");
    }

    #[test]
    fn command_uses_the_chosen_encoder_and_container() {
        let project = project(&[4.0], &[]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(4.0),
            PreciseSettings::default(),
        );

        let cmd = build_precise_command(
            &install_with(full_encoders()),
            &plan,
            &[PathBuf::from("C:/a.mp4")],
            Path::new("C:/out.part"),
            Path::new("C:/out.mp4"),
            true,
        )
        .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");

        assert!(joined.contains("-c:v libx264"), "{joined}");
        assert!(joined.contains("-crf 18"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-b:a 192k"), "{joined}");
        assert!(joined.contains("-filter_complex"), "{joined}");
        assert!(joined.contains("-map [vout]"), "{joined}");
        assert!(joined.contains("-map [aout]"), "{joined}");
        assert!(joined.contains("-f mp4"), "{joined}");
        assert!(joined.contains("-progress pipe:1"), "{joined}");
    }

    #[test]
    fn webm_output_uses_vp9_and_opus() {
        let project = project(&[4.0], &[]);
        let settings = PreciseSettings {
            codec: VideoCodecChoice::Vp9,
            ..PreciseSettings::default()
        };
        let plan = plan_precise(&project, Time::ZERO, Time::from_secs(4.0), settings);

        let cmd = build_precise_command(
            &install_with(full_encoders()),
            &plan,
            &[PathBuf::from("C:/a.mp4")],
            Path::new("C:/out.part"),
            Path::new("C:/out.webm"),
            true,
        )
        .unwrap();

        let joined: String = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(joined.contains("-c:v libvpx-vp9"), "{joined}");
        assert!(joined.contains("-c:a libopus"), "{joined}");
        assert!(joined.contains("-f webm"), "{joined}");
        assert!(!joined.contains("-c:a aac"), "webm cannot hold aac");
    }

    #[test]
    fn container_picks_the_codec() {
        let install = install_with(full_encoders());
        assert_eq!(
            VideoCodecChoice::for_container("webm", &install),
            VideoCodecChoice::Vp9
        );
        assert_eq!(
            VideoCodecChoice::for_container("mp4", &install),
            VideoCodecChoice::H264
        );
    }

    #[test]
    fn missing_vp9_encoder_is_an_error_not_a_silent_fallback() {
        let mut encoders = full_encoders();
        encoders.libvpx_vp9 = false;
        let install = install_with(encoders);
        assert_eq!(VideoCodecChoice::Vp9.encoder(&install), None);
    }

    #[test]
    fn h264_falls_back_when_libx264_is_absent() {
        let mut encoders = full_encoders();
        encoders.libx264 = false;
        let install = install_with(encoders);
        assert_eq!(VideoCodecChoice::H264.encoder(&install), Some("mpeg4"));
    }

    #[test]
    fn empty_range_is_blocked() {
        let project = Project::empty();
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(1.0),
            PreciseSettings::default(),
        );
        assert_eq!(plan.blocker, Some(ExportBlocker::NothingToExport));
    }
}
