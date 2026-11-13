//! Compatibility checks for lossless concatenation.
//!
//! The `concat` demuxer can only join segments that share a codec
//! configuration. When two sources differ in *any* of the parameters below,
//! FFmpeg either refuses or produces a file that plays back badly (wrong
//! resolution after the seam, audio drift, corrupt frames). Rather than let
//! that happen, the preflight compares the parameters and reports exactly what
//! differs so the UI can suggest re-encoding.

use crate::domain::Project;
use crate::media::probe::MediaInfo;
use crate::time::Time;

/// One incompatibility between two segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatIssue {
    /// Parameter name, e.g. `codec`, `width`.
    pub field: &'static str,
    /// Value in the reference segment.
    pub expected: String,
    /// Value in the differing segment.
    pub actual: String,
    /// 1-based position of the offending segment in export order.
    pub segment: usize,
}

impl CompatIssue {
    /// Human-readable single line, used in the dialog's detail list.
    pub fn describe(&self) -> String {
        format!(
            "#{}: {} {} != {}",
            self.segment, self.field, self.actual, self.expected
        )
    }

    /// i18n-friendly description built from keys.
    pub fn describe_localized(&self, tr: impl Fn(&'static str) -> String) -> String {
        format!(
            "#{} {}: {} → {}",
            self.segment,
            tr(self.field_i18n_key()),
            self.expected,
            self.actual
        )
    }

    fn field_i18n_key(&self) -> &'static str {
        match self.field {
            "video codec" => "export.field.video_codec",
            "profile" => "export.field.profile",
            "width" => "export.field.width",
            "height" => "export.field.height",
            "pixel format" => "export.field.pix_fmt",
            "frame rate" => "export.field.frame_rate",
            "sample aspect ratio" => "export.field.sar",
            "audio codec" => "export.field.audio_codec",
            "sample rate" => "export.field.sample_rate",
            "channels" => "export.field.channels",
            _ => "export.field.other",
        }
    }
}

/// Result of comparing every segment against the first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompatReport {
    pub issues: Vec<CompatIssue>,
}

impl CompatReport {
    pub fn is_compatible(&self) -> bool {
        self.issues.is_empty()
    }

    /// Distinct parameter names that differ, for a compact summary.
    pub fn failing_fields(&self) -> Vec<&'static str> {
        let mut fields: Vec<&'static str> = self.issues.iter().map(|i| i.field).collect();
        fields.sort_unstable();
        fields.dedup();
        fields
    }
}

/// Compares every segment against the first one.
///
/// Order matters: the first segment is the reference, matching what the
/// concat demuxer does in practice.
pub fn check_compat(project: &Project, source_ids: &[crate::SourceId]) -> CompatReport {
    let mut issues = Vec::new();

    let infos: Vec<Option<&MediaInfo>> = source_ids
        .iter()
        .map(|id| project.source(*id).map(|source| source.info.as_ref()))
        .collect();

    let Some(reference) = infos.first().copied().flatten() else {
        return CompatReport { issues };
    };

    for (index, info) in infos.iter().enumerate().skip(1) {
        let Some(info) = info else {
            issues.push(CompatIssue {
                field: "video codec",
                expected: reference.video.codec.clone(),
                actual: "missing".to_owned(),
                segment: index + 1,
            });
            continue;
        };
        issues.extend(compare(reference, info, index + 1));
    }

    CompatReport { issues }
}

/// Compares one segment against the reference.
fn compare(reference: &MediaInfo, other: &MediaInfo, segment: usize) -> Vec<CompatIssue> {
    let mut issues = Vec::new();

    let mut push = |field: &'static str, expected: String, actual: String| {
        if expected != actual {
            issues.push(CompatIssue {
                field,
                expected,
                actual,
                segment,
            });
        }
    };

    push(
        "video codec",
        reference.video.codec.clone(),
        other.video.codec.clone(),
    );
    push(
        "profile",
        reference.video.profile.clone().unwrap_or_default(),
        other.video.profile.clone().unwrap_or_default(),
    );
    push(
        "width",
        reference.video.width.to_string(),
        other.video.width.to_string(),
    );
    push(
        "height",
        reference.video.height.to_string(),
        other.video.height.to_string(),
    );
    push(
        "pixel format",
        reference.video.pix_fmt.clone().unwrap_or_default(),
        other.video.pix_fmt.clone().unwrap_or_default(),
    );
    push(
        "frame rate",
        frame_rate_key(&reference.video.frame_rate),
        frame_rate_key(&other.video.frame_rate),
    );
    push(
        "sample aspect ratio",
        reference
            .video
            .sample_aspect_ratio
            .clone()
            .unwrap_or_else(|| "1:1".to_owned()),
        other
            .video
            .sample_aspect_ratio
            .clone()
            .unwrap_or_else(|| "1:1".to_owned()),
    );

    match (&reference.audio, &other.audio) {
        (Some(a), Some(b)) => {
            push("audio codec", a.codec.clone(), b.codec.clone());
            push(
                "sample rate",
                a.sample_rate.to_string(),
                b.sample_rate.to_string(),
            );
            push("channels", a.channels.to_string(), b.channels.to_string());
        }
        // One has audio and the other does not: concatenation would either
        // drop audio or produce a file with a silent section that players
        // handle inconsistently.
        (Some(a), None) => issues.push(CompatIssue {
            field: "audio codec",
            expected: a.codec.clone(),
            actual: "none".to_owned(),
            segment,
        }),
        (None, Some(b)) => issues.push(CompatIssue {
            field: "audio codec",
            expected: "none".to_owned(),
            actual: b.codec.clone(),
            segment,
        }),
        (None, None) => {}
    }

    issues
}

fn frame_rate_key(rate: &crate::FrameRate) -> String {
    format!("{}/{}", rate.num, rate.den)
}

/// Convenience: collects the source ids used by a segment list, in order.
pub fn segment_sources(segments: &[super::Segment]) -> Vec<crate::SourceId> {
    segments.iter().map(|segment| segment.source_id).collect()
}

/// Rough estimate of how long an export will take, for the dialog.
///
/// Only used for the precise mode where encoding dominates; stream copy is
/// essentially I/O-bound and reported as `None`.
pub fn estimate_encode_seconds(duration: Time, speed: f32) -> Option<u64> {
    if speed <= 0.0 || duration.micros() <= 0 {
        return None;
    }
    Some((duration.as_secs_f64() / speed as f64).round().max(1.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaSource, SourceId};
    use crate::media::{probe, KeyframeIndex};

    fn info(name: &str) -> MediaInfo {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        probe::parse(std::path::Path::new(name), &json).unwrap()
    }

    fn project(sources: Vec<MediaInfo>) -> Project {
        let mut project = Project::empty();
        for mut media in sources {
            let id = project.alloc_source_id();
            media.path = std::path::PathBuf::from(format!("{}.mp4", id.0));
            let mut source = MediaSource::new(
                id,
                media,
                KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(4.0)]),
            );
            source.id = id;
            project.add_source(source);
        }
        project
    }

    fn ids(project: &Project) -> Vec<SourceId> {
        project.sources.iter().map(|s| s.id).collect()
    }

    #[test]
    fn identical_sources_are_compatible() {
        let project = project(vec![info("a.mp4"), info("b.mp4")]);
        let report = check_compat(&project, &ids(&project));
        assert!(report.is_compatible(), "{:?}", report.issues);
    }

    #[test]
    fn single_segment_is_always_compatible() {
        let project = project(vec![info("a.mp4")]);
        assert!(check_compat(&project, &ids(&project)).is_compatible());
    }

    #[test]
    fn different_resolution_is_reported() {
        let mut second = info("b.mp4");
        second.video.width = 640;
        let project = project(vec![info("a.mp4"), second]);

        let report = check_compat(&project, &ids(&project));
        assert!(!report.is_compatible());
        assert!(report.issues.iter().any(|i| i.field == "width"));
        assert_eq!(report.failing_fields(), vec!["width"]);
    }

    #[test]
    fn different_codec_is_reported() {
        let mut second = info("b.mp4");
        second.video.codec = "hevc".to_owned();
        let project = project(vec![info("a.mp4"), second]);

        let report = check_compat(&project, &ids(&project));
        assert!(report.issues.iter().any(|i| i.field == "video codec"));
    }

    #[test]
    fn missing_audio_on_one_segment_is_reported() {
        let mut second = info("b.mp4");
        second.audio = None;
        let project = project(vec![info("a.mp4"), second]);

        let report = check_compat(&project, &ids(&project));
        assert!(report.issues.iter().any(|i| i.field == "audio codec"));
    }

    #[test]
    fn reports_one_based_segment_numbers() {
        let mut second = info("b.mp4");
        second.video.width = 320;
        let project = project(vec![info("a.mp4"), second]);

        let report = check_compat(&project, &ids(&project));
        assert_eq!(report.issues[0].segment, 2);
    }

    #[test]
    fn describes_an_issue_readably() {
        let issue = CompatIssue {
            field: "width",
            expected: "1280".into(),
            actual: "640".into(),
            segment: 2,
        };
        assert_eq!(issue.describe(), "#2: width 640 != 1280");
    }

    #[test]
    fn failing_fields_are_deduplicated() {
        let mut second = info("b.mp4");
        second.video.width = 640;
        second.video.height = 480;
        let project = project(vec![info("a.mp4"), second, {
            let mut third = info("c.mp4");
            third.video.width = 320;
            third
        }]);

        let report = check_compat(&project, &ids(&project));
        assert_eq!(report.failing_fields(), vec!["height", "width"]);
    }

    #[test]
    fn frame_rate_key_uses_rational_form() {
        assert_eq!(
            frame_rate_key(&crate::FrameRate {
                num: 30000,
                den: 1001
            }),
            "30000/1001"
        );
    }

    #[test]
    fn estimate_scales_with_speed() {
        let duration = Time::from_secs(60.0);
        assert_eq!(estimate_encode_seconds(duration, 2.0), Some(30));
        assert_eq!(estimate_encode_seconds(duration, 0.5), Some(120));
        assert_eq!(estimate_encode_seconds(duration, 0.0), None);
    }

    #[test]
    fn segment_sources_preserves_order() {
        let project = project(vec![info("a.mp4"), info("b.mp4")]);
        let pieces =
            crate::media::export::timeline_pieces(&project, Time::ZERO, Time::from_secs(8.0))
                .unwrap();
        let flat = crate::media::export::only_segments(&pieces);
        let sources = segment_sources(&flat);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0], ids(&project)[0]);
        assert_eq!(sources[1], ids(&project)[1]);
    }
}
