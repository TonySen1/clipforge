//! Integration test for the scrub strip.
//!
//! Runs real FFmpeg, so it is ignored by default:
//!
//! ```text
//! cargo test --test thumbs_integration -- --ignored --nocapture
//! ```

use cf_core::media::{discover, probe, thumbs};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media")
        .join(name)
}

#[test]
#[ignore = "requires a real FFmpeg"]
fn builds_and_decodes_a_strip() {
    let (install, _) = discover(None).expect("FFmpeg is required");
    let path = fixture("sample_h264.mp4");

    let info = probe::probe(&install, &path).expect("probe");

    let strip = thumbs::build(
        &install,
        &path,
        &info,
        thumbs::STRIP_MAX_EDGE,
        thumbs::STRIP_BUDGET_BYTES,
    )
    .expect("building the strip");

    assert!(!strip.is_empty(), "strip should hold samples");
    assert!(
        strip.width().max(strip.height()) <= thumbs::STRIP_MAX_EDGE,
        "samples must stay within the edge cap"
    );
    assert!(
        strip.len() <= thumbs::STRIP_MAX_FRAMES,
        "sample count must stay within the cap"
    );

    // Every sample must decode to a real RGBA buffer.
    let index = strip
        .index_at_or_before(cf_core::Time::from_secs(1.0))
        .expect("every position resolves to a sample");
    let frame = strip.decode(index).expect("decoding a sample");
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!(
        frame.rgba.len(),
        frame.width as usize * frame.height as usize * 4
    );

    // Times are ordered, so the lookup is meaningful.
    let mut previous = cf_core::Time::ZERO;
    for probe_time in [0.0, 0.5, 1.5, 3.0] {
        let found = strip
            .index_at_or_before(cf_core::Time::from_secs(probe_time))
            .expect("every position resolves");
        let time = strip.time_at(found).unwrap();
        assert!(time >= previous || probe_time == 0.0);
        previous = time;
    }
}

#[test]
#[ignore = "requires a real FFmpeg"]
fn strip_is_dense_enough_for_short_clips() {
    let (install, _) = discover(None).expect("FFmpeg is required");
    let path = fixture("sample_h264.mp4");
    let info = probe::probe(&install, &path).expect("probe");

    let strip = thumbs::build(
        &install,
        &path,
        &info,
        thumbs::STRIP_MAX_EDGE,
        thumbs::STRIP_BUDGET_BYTES,
    )
    .expect("building the strip");

    // The whole point of the strip is showing a frame close to the pointer, so
    // a 4 s fixture must yield several samples, not one per keyframe.
    let seconds = info.duration.as_secs_f64();
    assert!(
        strip.len() as f64 >= seconds * 2.0,
        "expected at least 2 samples/s, got {} samples over {seconds}s",
        strip.len()
    );
}
