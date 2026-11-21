//! Preview frame decoding.
//!
//! Two decode strategies, because scrubbing and playback want different
//! things:
//!
//! * **Seek** (`decode_frame`) — one short-lived process per request. Used
//!   while dragging the playhead or stepping frames. ~125 ms for a cold seek,
//!   ~180 ms when a precise seek is needed (see the P2 measurements).
//! * **Stream** (`PlaybackProcess`) — one long-lived process reading
//!   `rawvideo` frames from a pipe at the source frame rate, used for playback.
//!
//! Both write RGBA at a height-capped resolution so the preview never uploads
//! a 4K texture.

use crate::error::CoreError;
use crate::media::ffmpeg::{hidden_command, FfmpegInstall};
use crate::media::probe::MediaInfo;
use crate::time::{FrameRate, Time};
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

/// Longest edge of a decoded preview frame, in pixels.
pub const MAX_PREVIEW_EDGE: u32 = 1280;

/// A decoded RGBA frame ready for texture upload.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Timeline position this frame was requested at.
    pub time: Time,
}

impl Frame {
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

/// Output geometry for a source, after fit-to-box and rotation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PreviewSize {
    pub width: u32,
    pub height: u32,
}

impl PreviewSize {
    pub const fn frame_bytes(self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// Computes the decoded size: rotate first, then fit inside `max_edge`.
///
/// Rotation is applied *before* fitting so a portrait phone video is sized by
/// its tall edge, which is what the user sees.
pub fn preview_size(info: &MediaInfo, max_edge: u32) -> PreviewSize {
    let (mut w, mut h) = info.display_size();
    if w == 0 || h == 0 {
        return PreviewSize {
            width: 0,
            height: 0,
        };
    }

    let longest = w.max(h);
    if longest > max_edge {
        // Scale to fit the box, then round *up* to an even number of pixels.
        // Even dimensions keep chroma-subsampled decoders happy and make the
        // pipe frame size exact; rounding up avoids clipping a row.
        let scale = max_edge as f64 / longest as f64;
        w = to_even(w as f64 * scale);
        h = to_even(h as f64 * scale);
    }

    PreviewSize {
        width: w,
        height: h,
    }
}

/// Rounds to the nearest even value of at least 2.
fn to_even(value: f64) -> u32 {
    let rounded = value.round().max(2.0) as u32;
    if rounded.is_multiple_of(2) {
        rounded
    } else {
        rounded + 1
    }
}

/// `-vf` filter chain that produces exactly `size`.
///
/// The transpose filters bake rotation into the pixels; a 90/270 rotation also
/// means the scale target is swapped, which `preview_size` already accounts
/// for.
pub(crate) fn video_filter(info: &MediaInfo, size: PreviewSize) -> String {
    let transpose = match info.video.rotation.rem_euclid(360) {
        90 => "transpose=clock",
        180 => "transpose=clock,transpose=clock",
        270 => "transpose=cclock",
        _ => "",
    };
    let scale = format!("scale={}:{}", size.width, size.height);

    if transpose.is_empty() {
        scale
    } else {
        format!("{transpose},{scale}")
    }
}

/// Decodes exactly one frame at `at`.
///
/// `precise` trades a second seek for frame-accurate output. Fast seeks snap
/// to the previous keyframe (up to the GOP length early), which is fine while
/// scrubbing but wrong for "previous/next frame".
pub fn decode_frame(
    install: &FfmpegInstall,
    path: &Path,
    info: &MediaInfo,
    at: Time,
    size: PreviewSize,
    precise: bool,
) -> Result<Frame, CoreError> {
    if size.width == 0 || size.height == 0 {
        return Err(CoreError::FfmpegBadOutput {
            program: "scale".to_owned(),
            detail: "preview size is zero".to_owned(),
        });
    }

    let seconds = at.as_secs_f64().max(0.0);
    let mut cmd = hidden_command(&install.ffmpeg);

    if precise {
        // Seek to just before the target so the second (accurate) seek only
        // has to decode a couple of frames.
        let coarse = (seconds - 2.0).max(0.0);
        let fine = seconds - coarse;
        cmd.args(["-v", "error", "-nostdin", "-ss"])
            .arg(format!("{coarse:.6}"))
            .arg("-i")
            .arg(path)
            .args(["-ss", &format!("{fine:.6}")]);
    } else {
        cmd.args(["-v", "error", "-nostdin", "-ss"])
            .arg(format!("{seconds:.6}"))
            .arg("-i")
            .arg(path);
    }

    let mut child = cmd
        .args([
            "-frames:v",
            "1",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            &video_filter(info, size),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CoreError::FfmpegSpawn {
            path: install.ffmpeg.clone(),
            source,
        })?;

    let mut rgba = vec![0u8; size.frame_bytes()];
    let read = read_exact_or_less(&mut child, &mut rgba);
    let status = child.wait();

    let filled = read?;
    if filled < rgba.len() {
        // A short read means the seek landed past the last frame, which is a
        // normal outcome for "end of file" requests, not a failure.
        return Err(CoreError::FfmpegBadOutput {
            program: "ffmpeg".to_owned(),
            detail: format!("frame at {} produced {filled} of {} bytes", at, rgba.len()),
        });
    }

    let _ = status;

    Ok(Frame {
        width: size.width,
        height: size.height,
        rgba,
        time: at,
    })
}

fn read_exact_or_less(child: &mut Child, buf: &mut [u8]) -> Result<usize, CoreError> {
    let Some(stdout) = child.stdout.as_mut() else {
        return Ok(0);
    };
    let mut reader = BufReader::new(stdout);
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(CoreError::FfmpegBadOutput {
                    program: "ffmpeg".to_owned(),
                    detail: err.to_string(),
                })
            }
        }
    }
    Ok(filled)
}

/// A running `ffmpeg` process streaming frames from `start` onward.
pub struct PlaybackProcess {
    child: Child,
    reader: BufReader<ChildStdout>,
    size: PreviewSize,
    frame_rate: FrameRate,
    /// Timestamp of the next frame this process will hand out.
    next_time: Time,
    /// Set once the process reaches EOF or fails.
    finished: bool,
}

impl PlaybackProcess {
    /// Spawns a decoder positioned at `start`.
    ///
    /// `-re` makes FFmpeg emit frames at wall-clock speed, so the UI does not
    /// need its own frame clock: reading a frame simply blocks until it is due.
    pub fn spawn(
        install: &FfmpegInstall,
        path: &Path,
        info: &MediaInfo,
        start: Time,
        size: PreviewSize,
    ) -> Result<Self, CoreError> {
        let seconds = start.as_secs_f64().max(0.0);
        let mut child = hidden_command(&install.ffmpeg)
            .args(["-v", "error", "-nostdin", "-re", "-ss"])
            .arg(format!("{seconds:.6}"))
            .arg("-i")
            .arg(path)
            .args([
                "-an",
                "-sn",
                "-dn",
                "-vf",
                &video_filter(info, size),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CoreError::FfmpegSpawn {
                path: install.ffmpeg.clone(),
                source,
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::FfmpegBadOutput {
                program: "ffmpeg".to_owned(),
                detail: "playback process has no stdout".to_owned(),
            })?;

        Ok(Self {
            child,
            reader: BufReader::with_capacity(size.frame_bytes() * 2, stdout),
            size,
            frame_rate: info.video.frame_rate,
            next_time: start,
            finished: false,
        })
    }

    pub fn size(&self) -> PreviewSize {
        self.size
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Blocking read of the next frame. Returns `None` at end of stream.
    ///
    /// Blocking is deliberate: with `-re` the process paces itself, so a
    /// blocked read *is* the correct wait and costs no polling.
    pub fn next_frame(&mut self, buf: &mut Vec<u8>) -> Option<Frame> {
        if self.finished {
            return None;
        }

        buf.resize(self.size.frame_bytes(), 0);
        let mut filled = 0;
        while filled < buf.len() {
            match self.reader.read(&mut buf[filled..]) {
                Ok(0) => {
                    self.finished = true;
                    return None;
                }
                Ok(n) => filled += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.finished = true;
                    return None;
                }
            }
        }

        let time = self.next_time;
        self.next_time = time.saturating_add(self.frame_rate.frame_duration());

        Some(Frame {
            width: self.size.width,
            height: self.size.height,
            rgba: std::mem::take(buf),
            time,
        })
    }
}

impl Drop for PlaybackProcess {
    fn drop(&mut self) {
        // Killing before waiting avoids a deadlock if the child is blocked
        // writing to a pipe we are no longer reading.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Kills a child process without leaving a zombie behind.
pub fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Builds a `Command` for a playback process; exposed for tests.
#[doc(hidden)]
pub fn playback_command(install: &FfmpegInstall) -> Command {
    hidden_command(&install.ffmpeg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe;

    fn info() -> MediaInfo {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        probe::parse(std::path::Path::new("sample_h264.mp4"), &json).unwrap()
    }

    #[test]
    fn fits_inside_the_box_by_the_longest_edge() {
        // 1280 wide exceeds a 720 box, so the whole frame scales down and the
        // short edge follows the aspect ratio.
        let size = preview_size(&info(), 720);
        assert_eq!(size.width, 720);
        assert_eq!(size.height, 406);
    }

    #[test]
    fn small_sources_are_not_upscaled() {
        let mut info = info();
        info.video.width = 320;
        info.video.height = 240;
        assert_eq!(
            preview_size(&info, 720),
            PreviewSize {
                width: 320,
                height: 240
            }
        );
    }

    #[test]
    fn caps_by_the_longest_edge() {
        let size = preview_size(&info(), 640);
        assert_eq!(size.width, 640);
        assert_eq!(size.height, 360);
        assert_eq!(size.frame_bytes(), 640 * 360 * 4);
    }

    #[test]
    fn portrait_is_sized_by_its_tall_edge() {
        let mut info = info();
        info.video.width = 1080;
        info.video.height = 1920;
        let size = preview_size(&info, 720);
        assert_eq!(
            size,
            PreviewSize {
                width: 406,
                height: 720
            }
        );
    }

    #[test]
    fn rotated_source_uses_display_orientation() {
        let mut info = info();
        info.video.width = 1080;
        info.video.height = 1920;
        info.video.rotation = 90;
        // Rotating a 1080x1920 portrait makes it landscape 1920x1080.
        let size = preview_size(&info, 720);
        assert_eq!(
            size,
            PreviewSize {
                width: 720,
                height: 406
            }
        );
    }

    #[test]
    fn filter_includes_transpose_only_when_needed() {
        let size = PreviewSize {
            width: 320,
            height: 240,
        };
        let mut info = info();
        assert_eq!(video_filter(&info, size), "scale=320:240");

        info.video.rotation = 90;
        assert_eq!(video_filter(&info, size), "transpose=clock,scale=320:240");

        info.video.rotation = 270;
        assert_eq!(video_filter(&info, size), "transpose=cclock,scale=320:240");

        info.video.rotation = 180;
        assert_eq!(
            video_filter(&info, size),
            "transpose=clock,transpose=clock,scale=320:240"
        );
    }

    #[test]
    fn zero_sized_source_is_handled() {
        let mut info = info();
        info.video.width = 0;
        info.video.height = 0;
        assert_eq!(
            preview_size(&info, 720),
            PreviewSize {
                width: 0,
                height: 0
            }
        );
    }

    #[test]
    fn odd_dimensions_round_to_even() {
        let mut info = info();
        info.video.width = 1919;
        info.video.height = 1081;
        let size = preview_size(&info, 720);
        assert_eq!(size.width % 2, 0, "width must be even");
        assert_eq!(size.height % 2, 0, "height must be even");
    }
}
