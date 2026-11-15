//! Version parsing and comparison for the update check.
//!
//! Kept in core so the comparison is testable without a network or a UI, and
//! so the "is this newer?" rule lives in one place instead of being duplicated
//! between the fetcher and the status bar.

use std::cmp::Ordering;

/// A `MAJOR.MINOR.PATCH` version, optionally prefixed with `v`.
///
/// Pre-release and build metadata are ignored: GitHub release tags for this
/// project are plain `vX.Y.Z`, and a pre-release never counts as an update.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parses `1.2.3` or `v1.2.3`. Trailing parts are optional, so `1.2` is
    /// accepted as `1.2.0`; anything with a leading `-`/`+` is treated as a
    /// pre-release and parsed up to the suffix.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        // Drop pre-release/build metadata before splitting.
        let core = text.split(['-', '+']).next().unwrap_or(text);

        let mut parts = core.split('.');
        let major = parse_part(parts.next()?)?;
        // A missing component is 0; a present but malformed one is an error,
        // so "1..3" is rejected rather than silently read as "1.0.3".
        let minor = match parts.next() {
            Some(text) => parse_part(text)?,
            None => 0,
        };
        let patch = match parts.next() {
            Some(text) => parse_part(text)?,
            None => 0,
        };

        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// True when `self` is strictly newer than `other`.
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.cmp_tuple() > other.cmp_tuple()
    }

    fn cmp_tuple(&self) -> (u32, u32, u32) {
        (self.major, self.minor, self.patch)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_tuple().cmp(&other.cmp_tuple())
    }
}

/// Parses one dotted component, rejecting empty or non-numeric input.
fn parse_part(text: &str) -> Option<u32> {
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Highest version among GitHub release tags.
///
/// Tags that do not parse (rolling `nightly` tags, drafts with odd names) are
/// skipped rather than aborting the whole check.
pub fn newest_release<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<(Version, &'a str)> {
    tags.into_iter()
        .filter_map(|tag| Version::parse(tag).map(|version| (version, tag)))
        .max_by_key(|(version, _)| *version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_prefixed_versions() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(Version::parse("v0.1.3"), Version::parse("0.1.3"));
        assert_eq!(Version::parse("V2.0.0"), Version::parse("2.0.0"));
    }

    #[test]
    fn missing_parts_default_to_zero() {
        assert_eq!(Version::parse("1"), Version::parse("1.0.0"));
        assert_eq!(Version::parse("1.2"), Version::parse("1.2.0"));
    }

    #[test]
    fn strips_prerelease_and_build_metadata() {
        assert_eq!(Version::parse("1.2.3-beta.1"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("1.2.3+abc"), Version::parse("1.2.3"));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("nightly"), None);
        assert_eq!(Version::parse("1.x.3"), None);
        assert_eq!(Version::parse("1..3"), None);
    }

    #[test]
    fn newer_is_strict() {
        let old = Version::parse("0.1.2").unwrap();
        let new = Version::parse("0.1.3").unwrap();
        assert!(new.is_newer_than(&old));
        assert!(!old.is_newer_than(&new));
        assert!(!old.is_newer_than(&old));
    }

    #[test]
    fn numeric_not_lexicographic_ordering() {
        let a = Version::parse("0.9.0").unwrap();
        let b = Version::parse("0.10.0").unwrap();
        assert!(b.is_newer_than(&a));
        assert_eq!(b.to_string(), "0.10.0");
    }

    #[test]
    fn picks_the_highest_parseable_tag() {
        let tags = ["v0.1.2", "nightly", "v0.1.10", "v0.1.3", "broken"];
        let (version, tag) = newest_release(tags).unwrap();
        assert_eq!(version, Version::parse("0.1.10").unwrap());
        assert_eq!(tag, "v0.1.10");
    }

    #[test]
    fn no_parseable_tags_yields_none() {
        assert_eq!(newest_release(["nightly", "latest"]), None);
    }
}
