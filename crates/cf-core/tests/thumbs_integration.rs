//! Integration test for the keyframe scrub strip.
//!
//! Runs real FFmpeg, so it is ignored by default:
//!
//! ```text
//! cargo test --test thumbs_integration -- --ignored --nocapture
//! ```

use cf_core::media::{discover, keyframes, probe, thumbs};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media")
        .join(name)
}

#[test]
#[ignore = "requires a real FFmpeg"]
fn builds_a_strip_from_real_keyframes() {
    let (install, _) = discover(None).expect("FFmpeg is required");
    let path = fixture("sample_h264.mp4");

    let info = probe::probe(&install, &path).expect("probe");
    let index = keyframes::probe(&install, &path).expect("keyframes");
    assert!(!index.is_empty(), "fixture should have keyframes");

    let strip = thumbs::build(
        &install,
        &path,
        &info,
        &index,
        thumbs::STRIP_MAX_EDGE,
        thumbs::STRIP_BUDGET_BYTES,
    )
    .expect("building the strip");

    assert!(
        !strip.is_empty(),
        "strip should contain at least one decoded keyframe"
    );
    assert!(
        strip.len() <= index.len(),
        "the strip never holds more frames than there are keyframes"
    );

    // Every frame must be a complete RGBA buffer at the strip size.
    let frame = strip.at_or_before(cf_core::Time::from_secs(1.0)).unwrap();
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!(
        frame.rgba.len(),
        frame.width as usize * frame.height as usize * 4
    );
    assert!(
        frame.width.max(frame.height) <= thumbs::STRIP_MAX_EDGE,
        "strip frames must stay within the edge cap"
    );

    // Times stay ordered so `at_or_before` is a valid lookup.
    let mut previous = cf_core::Time::ZERO;
    for probe_time in [0.0, 0.5, 1.5, 3.0] {
        let found = strip
            .at_or_before(cf_core::Time::from_secs(probe_time))
            .expect("every position resolves to a frame");
        assert!(found.time >= previous || probe_time == 0.0);
        previous = found.time;
    }
}
