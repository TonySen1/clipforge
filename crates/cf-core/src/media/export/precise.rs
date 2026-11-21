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

/// How hard the encoder works per frame.
///
/// Re-encoding a timeline is CPU-bound, and the presets trade speed for
/// compression efficiency at the same CRF. `Fast` is the default because an
/// export that finishes quickly is worth more than a few percent of bitrate.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum EncoderSpeed {
    #[default]
    Fast,
    Balanced,
    Quality,
}

impl EncoderSpeed {
    pub const ALL: [Self; 3] = [Self::Fast, Self::Balanced, Self::Quality];

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Fast => "export.speed.fast",
            Self::Balanced => "export.speed.balanced",
            Self::Quality => "export.speed.quality",
        }
    }

    /// One-line explanation shown under the speed chips.
    pub fn hint_key(self) -> &'static str {
        match self {
            Self::Fast => "export.speed.fast_hint",
            Self::Balanced => "export.speed.balanced_hint",
            Self::Quality => "export.speed.quality_hint",
        }
    }

    /// `-preset` value for libx264.
    fn x264_preset(self) -> &'static str {
        match self {
            Self::Fast => "veryfast",
            Self::Balanced => "fast",
            Self::Quality => "medium",
        }
    }

    /// `-preset` value for libx265.
    fn x265_preset(self) -> &'static str {
        match self {
            Self::Fast => "veryfast",
            Self::Balanced => "fast",
            Self::Quality => "medium",
        }
    }

    /// `-cpu-used` for libvpx-vp9 (0 = slowest, 8 = fastest).
    fn vp9_cpu_used(self) -> u32 {
        match self {
            Self::Fast => 6,
            Self::Balanced => 4,
            Self::Quality => 2,
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
    /// Encoder effort; see [`EncoderSpeed`].
    pub speed: EncoderSpeed,
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
            speed: EncoderSpeed::default(),
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

    /// Timeline position the plan starts at.
    ///
    /// Pieces cover the export window contiguously, so the first piece's start
    /// is the window start.
    pub fn start(&self) -> Time {
        self.pieces
            .first()
            .map(|piece| match piece {
                TimelinePiece::Clip(segment) => segment.timeline_start,
                TimelinePiece::Gap { start, .. } => *start,
            })
            .unwrap_or(Time::ZERO)
    }

    /// Cuts this plan down to `[start, end)`, adjusting source in/out points.
    ///
    /// Used to split a long export into chunks that can be encoded in parallel.
    /// The graph only cares about durations and source times, so the pieces keep
    /// their absolute timeline positions but the command never reads them.
    pub fn slice(&self, start: Time, end: Time) -> PrecisePlan {
        let mut pieces = Vec::new();
        for piece in &self.pieces {
            match piece {
                TimelinePiece::Clip(segment) => {
                    let from = segment.timeline_start.max(start);
                    let to = segment.timeline_end.min(end);
                    if from >= to {
                        continue;
                    }
                    let shift = from.saturating_sub(segment.timeline_start);
                    let source_in = segment.source_in.saturating_add(shift);
                    pieces.push(TimelinePiece::Clip(Segment {
                        source_id: segment.source_id,
                        source_in,
                        source_out: source_in.saturating_add(to.saturating_sub(from)),
                        timeline_start: from,
                        timeline_end: to,
                    }));
                }
                TimelinePiece::Gap {
                    start: gap_start,
                    end: gap_end,
                } => {
                    let from = (*gap_start).max(start);
                    let to = (*gap_end).min(end);
                    if from >= to {
                        continue;
                    }
                    pieces.push(TimelinePiece::Gap {
                        start: from,
                        end: to,
                    });
                }
            }
        }

        PrecisePlan {
            pieces,
            sources_with_audio: self.sources_with_audio.clone(),
            duration: end.saturating_sub(start),
            width: self.width,
            height: self.height,
            frame_rate: self.frame_rate,
            settings: self.settings.clone(),
            blocker: None,
        }
    }
}

/// One piece of a smart-cut export: either a lossless copy or a short
/// re-encoded span between a cut point and a keyframe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartPiece {
    pub source_id: crate::SourceId,
    pub source_in: Time,
    pub source_out: Time,
    /// True when this span must be re-encoded (it is not keyframe-aligned).
    pub encode: bool,
}

/// Encoder parameters needed to re-encode smart-cut boundary pieces so they
/// concat cleanly with the copied ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartSource {
    pub id: crate::SourceId,
    pub codec: VideoCodecChoice,
    pub width: u32,
    pub height: u32,
    pub frame_rate: crate::FrameRate,
}

/// A stream-copy fast path for precise exports.
///
/// Instead of re-encoding the whole timeline, keyframe-aligned spans are copied
/// and only the short pieces between a cut point and the nearest keyframe are
/// re-encoded. This mirrors LosslessCut's "smart cut".
#[derive(Clone, Debug, PartialEq)]
pub struct SmartCutPlan {
    /// Pieces in export order.
    pub pieces: Vec<SmartPiece>,
    /// Per-source encoder parameters for the re-encoded pieces.
    pub sources: Vec<SmartSource>,
    /// How much of the export is copied rather than encoded.
    pub copied: Time,
    pub duration: Time,
}

impl SmartCutPlan {
    pub fn source(&self, id: crate::SourceId) -> Option<&SmartSource> {
        self.sources.iter().find(|source| source.id == id)
    }
}

/// Builds a smart-cut plan when the timeline allows stream copying.
///
/// Returns `None` when the timeline cannot use the fast path: gaps, mixed
/// geometry or frame rates, rotated sources, non-8-bit pixel formats, missing
/// keyframes, codecs other than H.264/H.265, or too little copied material to
/// be worth it. Callers fall back to a full re-encode.
pub fn plan_smart_cut(
    project: &Project,
    start: Time,
    end: Time,
    settings: &PreciseSettings,
) -> Option<SmartCutPlan> {
    let pieces = super::timeline_pieces(project, start, end).ok()?;
    if pieces.is_empty() || super::has_gaps(&pieces) {
        return None;
    }

    let mut smart_sources: Vec<SmartSource> = Vec::new();
    let mut reference: Option<(u32, u32, crate::FrameRate)> = None;

    for segment in super::only_segments(&pieces) {
        let source = project.source(segment.source_id)?;
        let info = &source.info;
        if info.video.rotation.rem_euclid(360) != 0 {
            return None;
        }
        if info.video.pix_fmt.as_deref() != Some("yuv420p") {
            return None;
        }
        let codec = match info.video.codec.as_str() {
            "h264" => VideoCodecChoice::H264,
            "hevc" | "h265" => VideoCodecChoice::H265,
            _ => return None,
        };
        if source.keyframes.is_degraded() {
            return None;
        }
        let (width, height) = info.display_size();
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return None;
        }
        if info.video.frame_rate.fps() <= 0.0 {
            return None;
        }

        // Concatenating copied pieces only works when every source shares the
        // coded geometry and rate.
        match reference {
            None => reference = Some((width, height, info.video.frame_rate)),
            Some((w, h, rate)) => {
                if (w, h, rate) != (width, height, info.video.frame_rate) {
                    return None;
                }
            }
        }

        // A user-forced size or rate would need real resampling.
        if settings.size.is_some_and(|size| size != (width, height)) {
            return None;
        }
        if settings
            .frame_rate
            .is_some_and(|rate| rate != info.video.frame_rate)
        {
            return None;
        }

        if !smart_sources.iter().any(|item| item.id == source.id) {
            smart_sources.push(SmartSource {
                id: source.id,
                codec,
                width,
                height,
                frame_rate: info.video.frame_rate,
            });
        }
    }

    let mut smart_pieces: Vec<SmartPiece> = Vec::new();
    let mut copied = Time::ZERO;

    for segment in super::only_segments(&pieces) {
        let source = project.source(segment.source_id)?;
        let keyframes = source.keyframes.as_slice();
        if keyframes.is_empty() {
            return None;
        }
        let frame = source.info.video.frame_rate.frame_duration();
        let half_frame = Time(frame.micros() / 2);

        let start_is_key = keyframes
            .iter()
            .copied()
            .find(|key| *key <= segment.source_in)
            .is_some_and(|key| segment.source_in.saturating_sub(key) <= half_frame);

        let copy_start = if start_is_key {
            segment.source_in
        } else {
            match keyframes
                .iter()
                .copied()
                .find(|key| *key >= segment.source_in)
            {
                Some(key) if key < segment.source_out => key,
                _ => {
                    // The whole segment sits inside one GOP.
                    push_encode(
                        &mut smart_pieces,
                        &segment,
                        segment.source_in,
                        segment.source_out,
                    );
                    continue;
                }
            }
        };

        let end_is_key = keyframes
            .iter()
            .copied()
            .find(|key| *key <= segment.source_out)
            .is_some_and(|key| segment.source_out.saturating_sub(key) <= half_frame);

        let copy_end = if end_is_key {
            segment.source_out
        } else {
            match keyframes
                .iter()
                .copied()
                .rev()
                .find(|key| *key <= segment.source_out)
            {
                Some(key) if key > copy_start => key,
                _ => {
                    push_encode(
                        &mut smart_pieces,
                        &segment,
                        segment.source_in,
                        segment.source_out,
                    );
                    continue;
                }
            }
        };

        if copy_end.saturating_sub(copy_start) < frame {
            push_encode(
                &mut smart_pieces,
                &segment,
                segment.source_in,
                segment.source_out,
            );
            continue;
        }

        if copy_start > segment.source_in {
            push_encode(&mut smart_pieces, &segment, segment.source_in, copy_start);
        }
        smart_pieces.push(SmartPiece {
            source_id: segment.source_id,
            source_in: copy_start,
            source_out: copy_end,
            encode: false,
        });
        copied = copied.saturating_add(copy_end.saturating_sub(copy_start));
        if segment.source_out > copy_end {
            push_encode(&mut smart_pieces, &segment, copy_end, segment.source_out);
        }
    }

    if smart_pieces.is_empty() || copied < Time::from_secs(2.0) {
        return None;
    }

    Some(SmartCutPlan {
        pieces: smart_pieces,
        sources: smart_sources,
        copied,
        duration: end.saturating_sub(start),
    })
}

fn push_encode(pieces: &mut Vec<SmartPiece>, segment: &Segment, from: Time, to: Time) {
    if to <= from {
        return;
    }
    pieces.push(SmartPiece {
        source_id: segment.source_id,
        source_in: from,
        source_out: to,
        encode: true,
    });
}

/// Which streams a filter graph should produce.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GraphStreams {
    Both,
    VideoOnly,
    /// Video is dropped so a single continuous audio pass can be encoded for a
    /// chunked export, avoiding gaps at chunk joins.
    AudioOnly,
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
///
/// Inputs are expected to be pre-seeked with `-ss <source_in>` by
/// [`build_precise_command`], so every trim starts at zero. Doing the cut as an
/// *input seek* is what keeps exports fast: a `trim` on an unseeked input makes
/// FFmpeg decode the whole prefix first, which on a long recording costs more
/// than the encode itself.
pub fn build_filter_graph(plan: &PrecisePlan) -> String {
    build_filter_graph_with(plan, GraphStreams::Both)
}

/// Like [`build_filter_graph`], but can emit video only or audio only.
///
/// The split exists for parallel exports: video is encoded in chunks that run
/// concurrently, while audio is encoded once, continuously, so chunk joins do
/// not introduce AAC gaps.
pub fn build_filter_graph_with(plan: &PrecisePlan, streams: GraphStreams) -> String {
    let want_video = streams != GraphStreams::AudioOnly;
    let want_audio = streams != GraphStreams::VideoOnly;

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

                if want_video {
                    let v = format!("v{position}");
                    chains.push(format!(
                        "[{input_index}:v]trim=start=0:duration={dur:.6},setpts=PTS-STARTPTS,\
scale={w}:{h}:force_original_aspect_ratio=decrease,\
pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}[{v}]",
                        input_index = input_index,
                        dur = duration,
                        w = plan.width,
                        h = plan.height,
                        fps = plan.frame_rate.fps(),
                        v = v
                    ));
                    video_labels.push(v);
                }

                if want_audio {
                    let a = format!("a{position}");
                    // A segment whose source has no audio stream cannot link
                    // `[i:a]`; synthesise silence of the same length instead.
                    if plan.has_audio(segment.source_id) {
                        chains.push(format!(
                            "[{input_index}:a]atrim=start=0:duration={dur:.6},asetpts=PTS-STARTPTS,\
aresample=async=1:first_pts=0[{a}]",
                            input_index = input_index,
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
                }

                input_index += 1;
            }
            TimelinePiece::Gap { start, end } => {
                // A gap has no input; synthesise it from lavfi sources.
                let duration = end.saturating_sub(*start).as_secs_f64();
                if duration <= 0.0 {
                    continue;
                }

                if want_video {
                    let v = format!("v{position}");
                    chains.push(format!(
                        "color=c=black:s={w}x{h}:r={fps}:d={dur:.6},setsar=1[{v}]",
                        w = plan.width,
                        h = plan.height,
                        fps = plan.frame_rate.fps(),
                        dur = duration,
                        v = v
                    ));
                    video_labels.push(v);
                }
                if want_audio {
                    let a = format!("a{position}");
                    chains.push(format!(
                        "anullsrc=r={rate}:cl=stereo,atrim=duration={dur:.6}[{a}]",
                        rate = 48_000,
                        dur = duration,
                        a = a
                    ));
                    audio_labels.push(a);
                }
            }
        }
    }

    let inputs: String = match streams {
        GraphStreams::Both => video_labels
            .iter()
            .zip(audio_labels.iter())
            .map(|(v, a)| format!("[{v}][{a}]"))
            .collect(),
        GraphStreams::VideoOnly => video_labels.iter().map(|v| format!("[{v}]")).collect(),
        GraphStreams::AudioOnly => audio_labels.iter().map(|a| format!("[{a}]")).collect(),
    };

    let n = match streams {
        GraphStreams::AudioOnly => audio_labels.len(),
        _ => video_labels.len(),
    };
    let (v_flag, a_flag, output_labels) = match streams {
        GraphStreams::Both => (1, 1, "[vout][aout]"),
        GraphStreams::VideoOnly => (1, 0, "[vout]"),
        GraphStreams::AudioOnly => (0, 1, "[aout]"),
    };

    chains.push(format!(
        "{inputs}concat=n={n}:v={v_flag}:a={a_flag}{output_labels}"
    ));
    chains.join(";")
}

/// Builds the full `ffmpeg` command for a precise export.
///
/// `segment_inputs` is passed separately from the plan so the caller can supply
/// the real input paths in export order.
pub fn build_precise_command(
    install: &FfmpegInstall,
    plan: &PrecisePlan,
    segment_inputs: &[PathBuf],
    temp_output: &Path,
    output_for_container: &Path,
    has_audio_anywhere: bool,
) -> Result<Command, ExportBlocker> {
    let container = super::container_for(output_for_container)
        .ok_or_else(|| ExportBlocker::UnsupportedContainer("unknown container".to_owned()))?;
    build_precise_command_for(
        install,
        plan,
        segment_inputs,
        temp_output,
        container,
        has_audio_anywhere,
        GraphStreams::Both,
    )
}

/// Builds a precise-export command for an explicit container and stream set.
///
/// Chunked exports use [`GraphStreams::VideoOnly`] with `"mpegts"` for the
/// pieces and [`GraphStreams::AudioOnly`] with `"ipod"` for the single audio
/// pass, then mux the results.
#[allow(clippy::too_many_arguments)]
pub fn build_precise_command_for(
    install: &FfmpegInstall,
    plan: &PrecisePlan,
    segment_inputs: &[PathBuf],
    temp_output: &Path,
    container: &str,
    has_audio_anywhere: bool,
    streams: GraphStreams,
) -> Result<Command, ExportBlocker> {
    let encoder =
        if streams == GraphStreams::AudioOnly {
            None
        } else {
            Some(plan.settings.codec.encoder(install).ok_or_else(|| {
                ExportBlocker::UnsupportedContainer("encoder unavailable".to_owned())
            })?)
        };

    let mut cmd = hidden_command(&install.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y"]);

    // Seek each input to its in point. Without this the trim filter decodes
    // the entire prefix of every source before the cut, which dominates export
    // time for clips taken from late in a long recording.
    for (segment, path) in plan.segments().iter().zip(segment_inputs.iter()) {
        cmd.arg("-ss")
            .arg(format!("{:.6}", segment.source_in.as_secs_f64()));
        cmd.arg("-i").arg(path);
    }

    // Gap synthesis needs a silent-audio template if any segment lacks audio.
    if streams != GraphStreams::VideoOnly {
        let needs_silence = plan.has_gaps() || !has_audio_anywhere;
        if needs_silence {
            cmd.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]);
        }
    }

    let graph = build_filter_graph_with(plan, streams);
    cmd.args(["-filter_complex", &graph]);
    match streams {
        GraphStreams::Both => {
            cmd.args(["-map", "[vout]", "-map", "[aout]"]);
        }
        GraphStreams::VideoOnly => {
            cmd.args(["-map", "[vout]", "-an"]);
        }
        GraphStreams::AudioOnly => {
            cmd.args(["-map", "[aout]", "-vn"]);
        }
    }

    if let Some(encoder) = encoder {
        cmd.args(["-c:v", encoder]);
        match plan.settings.codec {
            VideoCodecChoice::Vp9 => {
                cmd.args(["-crf", &plan.settings.crf.to_string()]);
                cmd.args(["-b:v", "0"]);
                cmd.args(["-row-mt", "1"]);
                // The default VP9 settings are orders of magnitude slower than
                // the other encoders; `good` + `cpu-used` is the standard dial.
                cmd.args(["-deadline", "good"]);
                cmd.args(["-cpu-used", &plan.settings.speed.vp9_cpu_used().to_string()]);
            }
            VideoCodecChoice::H265 => {
                cmd.args(["-crf", &plan.settings.crf.to_string()]);
                cmd.args(["-preset", plan.settings.speed.x265_preset()]);
                // libx265 writes its banner to stderr, which pollutes the log.
                cmd.args(["-x265-params", "log-level=error"]);
            }
            VideoCodecChoice::H264 => {
                cmd.args(["-crf", &plan.settings.crf.to_string()]);
                cmd.args(["-preset", plan.settings.speed.x264_preset()]);
                cmd.args(["-pix_fmt", "yuv420p"]);
            }
        }
    }

    if streams != GraphStreams::VideoOnly {
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

        // The in point is applied as an input seek, so every trim starts at 0.
        assert!(graph.contains("trim=start=0:duration=3.000000"), "{graph}");
        assert!(graph.contains("setpts=PTS-STARTPTS"), "{graph}");
        assert!(graph.contains("scale=1280:720"), "{graph}");
        assert!(graph.contains("pad=1280:720"), "{graph}");
        assert!(graph.contains("setsar=1"), "{graph}");
        assert!(graph.contains("atrim=start=0:duration=3.000000"), "{graph}");
        assert!(graph.contains("concat=n=2:v=1:a=1[vout][aout]"), "{graph}");
    }

    #[test]
    fn command_seeks_each_input_to_its_in_point() {
        let project = project(&[4.0, 3.0], &[]);
        // Start one second in: the first segment's in point is 1 s, and the
        // second segment keeps its own (0 s) in point.
        let plan = plan_precise(
            &project,
            Time::from_secs(1.0),
            Time::from_secs(6.0),
            PreciseSettings::default(),
        );

        let cmd = build_precise_command(
            &install_with(full_encoders()),
            &plan,
            &[PathBuf::from("C:/a.mp4"), PathBuf::from("C:/b.mp4")],
            Path::new("C:/out.part"),
            Path::new("C:/out.mp4"),
            true,
        )
        .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // Each `-ss` must be immediately before its own `-i`.
        assert_eq!(args[4], "1.000000", "{args:?}");
        assert_eq!(args[5], "-i");
        assert_eq!(args[6], "C:/a.mp4");
        assert_eq!(args[7], "-ss");
        assert_eq!(args[8], "0.000000");
        assert_eq!(args[9], "-i");
        assert_eq!(args[10], "C:/b.mp4");
    }

    #[test]
    fn smart_cut_copies_between_keyframes_and_encodes_the_edges() {
        let mut project = Project::empty();
        let id = project.alloc_source_id();
        let mut media = info("a.mp4", 10.0);
        media.duration = Time::from_secs(10.0);
        let mut source = MediaSource::new(
            id,
            media,
            KeyframeIndex::new(vec![
                Time::ZERO,
                Time::from_secs(2.0),
                Time::from_secs(4.0),
                Time::from_secs(6.0),
                Time::from_secs(8.0),
            ]),
        );
        source.id = id;
        let clip_id = project.add_source(source);

        let plan = plan_smart_cut(
            &project,
            Time::from_secs(1.0),
            Time::from_secs(7.0),
            &PreciseSettings::default(),
        )
        .expect("smart cut should apply");
        assert_eq!(plan.pieces.len(), 3, "{:?}", plan.pieces);
        assert!(plan.pieces[0].encode);
        assert_eq!(plan.pieces[0].source_in, Time::from_secs(1.0));
        assert_eq!(plan.pieces[0].source_out, Time::from_secs(2.0));
        assert!(!plan.pieces[1].encode);
        assert_eq!(plan.pieces[1].source_in, Time::from_secs(2.0));
        assert_eq!(plan.pieces[1].source_out, Time::from_secs(6.0));
        assert!(plan.pieces[2].encode);
        assert_eq!(plan.pieces[2].source_in, Time::from_secs(6.0));
        assert_eq!(plan.pieces[2].source_out, Time::from_secs(7.0));
        assert_eq!(plan.copied, Time::from_secs(4.0));
        let _ = clip_id;
    }

    #[test]
    fn smart_cut_skips_gaps_and_small_savings() {
        // A gap rules the fast path out: gap filling uses synthetic sources.
        let project = project(&[3.0, 3.0], &[0.0, 5.0]);
        assert!(plan_smart_cut(
            &project,
            Time::ZERO,
            Time::from_secs(8.0),
            &PreciseSettings::default()
        )
        .is_none());

        // A single clip entirely inside one GOP has nothing to copy.
        let mut project = Project::empty();
        let id = project.alloc_source_id();
        let mut media = info("a.mp4", 10.0);
        media.duration = Time::from_secs(10.0);
        let mut source = MediaSource::new(id, media, KeyframeIndex::new(vec![Time::ZERO]));
        source.id = id;
        project.add_source(source);
        assert!(plan_smart_cut(
            &project,
            Time::from_secs(1.0),
            Time::from_secs(3.0),
            &PreciseSettings::default()
        )
        .is_none());
    }

    #[test]
    fn smart_cut_rejects_rotated_sources() {
        let mut project = Project::empty();
        let id = project.alloc_source_id();
        let mut media = info("a.mp4", 10.0);
        media.video.rotation = 90;
        let mut source = MediaSource::new(
            id,
            media,
            KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(2.0)]),
        );
        source.id = id;
        project.add_source(source);
        assert!(plan_smart_cut(
            &project,
            Time::ZERO,
            Time::from_secs(6.0),
            &PreciseSettings::default()
        )
        .is_none());
    }

    #[test]
    fn slicing_keeps_durations_and_source_times_aligned() {
        let project = project(&[4.0, 3.0], &[]);
        let plan = plan_precise(
            &project,
            Time::from_secs(1.0),
            Time::from_secs(6.0),
            PreciseSettings::default(),
        );

        // Second half of the export starts 2.5 s into the first plan.
        let slice = plan.slice(Time::from_secs(3.5), Time::from_secs(6.0));
        assert_eq!(slice.duration, Time::from_secs(2.5));

        let segments = slice.segments();
        assert_eq!(segments.len(), 2);
        // First segment: in the full plan it starts at source 1 s; the slice
        // starts 2.5 s later, so source_in must be 3.5 s.
        assert_eq!(segments[0].source_in, Time::from_secs(3.5));
        assert_eq!(segments[0].duration(), Time::from_secs(0.5));
        // Second segment starts exactly at the timeline cut.
        assert_eq!(segments[1].source_in, Time::ZERO);
        assert_eq!(segments[1].duration(), Time::from_secs(2.0));

        let total: Time = segments
            .iter()
            .fold(Time::ZERO, |acc, s| acc.saturating_add(s.duration()));
        assert_eq!(total, Time::from_secs(2.5));
    }

    #[test]
    fn slicing_preserves_gaps() {
        let project = project(&[3.0, 3.0], &[0.0, 5.0]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(8.0),
            PreciseSettings::default(),
        );
        let slice = plan.slice(Time::from_secs(4.0), Time::from_secs(7.0));
        assert!(slice.has_gaps(), "{:?}", slice.pieces);
        assert_eq!(slice.duration, Time::from_secs(3.0));
    }

    #[test]
    fn video_only_graph_has_no_audio_links() {
        let project = project(&[4.0], &[]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(4.0),
            PreciseSettings::default(),
        );
        let graph = build_filter_graph_with(&plan, GraphStreams::VideoOnly);
        assert!(!graph.contains(":a]"), "{graph}");
        assert!(graph.contains("concat=n=1:v=1:a=0[vout]"), "{graph}");
        assert!(graph.contains("trim=start=0"), "{graph}");
    }

    #[test]
    fn audio_only_graph_has_no_video_links() {
        let project = project(&[4.0], &[]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(4.0),
            PreciseSettings::default(),
        );
        let graph = build_filter_graph_with(&plan, GraphStreams::AudioOnly);
        assert!(!graph.contains(":v]"), "{graph}");
        assert!(graph.contains("concat=n=1:v=0:a=1[aout]"), "{graph}");
        assert!(graph.contains("atrim=start=0"), "{graph}");
    }

    #[test]
    fn audio_only_command_drops_video_encoder() {
        let project = project(&[4.0], &[]);
        let plan = plan_precise(
            &project,
            Time::ZERO,
            Time::from_secs(4.0),
            PreciseSettings::default(),
        );
        let cmd = build_precise_command_for(
            &install_with(full_encoders()),
            &plan,
            &[PathBuf::from("C:/a.mp4")],
            Path::new("C:/out.m4a"),
            "ipod",
            true,
            GraphStreams::AudioOnly,
        )
        .unwrap();
        let joined: String = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!joined.contains("-c:v"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-map [aout]"), "{joined}");
        assert!(joined.contains("-f ipod"), "{joined}");
    }

    #[test]
    fn speed_choices_map_to_encoder_presets() {
        let project = project(&[4.0], &[]);
        let plan_for = |speed| {
            let settings = PreciseSettings {
                speed,
                ..PreciseSettings::default()
            };
            let plan = plan_precise(&project, Time::ZERO, Time::from_secs(4.0), settings);
            let cmd = build_precise_command(
                &install_with(full_encoders()),
                &plan,
                &[PathBuf::from("C:/a.mp4")],
                Path::new("C:/out.part"),
                Path::new("C:/out.mp4"),
                true,
            )
            .unwrap();
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };

        assert!(plan_for(EncoderSpeed::Fast).contains("-preset veryfast"));
        assert!(plan_for(EncoderSpeed::Balanced).contains("-preset fast"));
        assert!(plan_for(EncoderSpeed::Quality).contains("-preset medium"));
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
        assert!(joined.contains("-preset veryfast"), "{joined}");
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
