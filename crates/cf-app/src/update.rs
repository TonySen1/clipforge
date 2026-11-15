//! Update check against GitHub Releases.
//!
//! Runs once shortly after launch on a detached thread, then never again for
//! this session. The UI polls `try_recv` like the other workers so the check
//! never blocks a frame.
//!
//! Only *checking* is automated. Finding a newer tag surfaces a quiet status
//! bar entry that opens the release page; installing stays a manual download,
//! which keeps the app free of write access to its own installation directory.

use cf_core::version::Version;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// `/releases/latest` answers with a 302 whose `Location` ends in the tag
/// (`.../releases/tag/v0.1.3`).
///
/// Deliberately not the JSON API: unauthenticated `api.github.com` allows 60
/// requests per hour *per IP*, so every user behind the same NAT shares one
/// budget and a busy network would make the check fail silently. This
/// redirect is served by the normal web host and is not rate limited.
const LATEST_RELEASE: &str = "https://github.com/TonySen1/clipforge/releases/latest";

/// Release page opened when the user acts on the notice.
pub const RELEASES_PAGE: &str = "https://github.com/TonySen1/clipforge/releases/latest";

/// How long to wait after launch before the first request.
///
/// Import and preview setup are the priority at startup; delaying the request
/// keeps it off the critical path and out of the way of a slow network.
const STARTUP_DELAY: Duration = Duration::from_secs(6);

/// Hard cap on the request, so a network stall cannot pin a thread forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What the check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateEvent {
    /// A newer release exists. `tag` is the raw tag (`v0.1.4`).
    Available { tag: String, version: Version },
    /// Already current, or nothing newer to report. Silently ignored.
    UpToDate,
    /// The check failed. Logged, never shown: a machine that is offline should
    /// not get an error banner about updates.
    Failed(String),
}

/// Spawns the check. Returns a receiver the UI polls.
///
/// The current version comes from `CARGO_PKG_VERSION`, so a build can never
/// claim to be newer than it is.
pub fn check_async(ctx: egui::Context) -> Receiver<UpdateEvent> {
    let (tx, rx) = mpsc::channel();
    let current = Version::parse(env!("CARGO_PKG_VERSION"));

    std::thread::Builder::new()
        .name("clipforge-update".to_owned())
        .spawn(move || {
            std::thread::sleep(STARTUP_DELAY);
            let event = match current {
                Some(current) => fetch(current),
                None => UpdateEvent::Failed("unparsable CARGO_PKG_VERSION".to_owned()),
            };
            // A closed receiver just means the window is gone.
            let _ = tx.send(event);
            ctx.request_repaint();
        })
        .expect("spawning the update check");

    rx
}

/// Performs the request and compares the tag in the redirect with `current`.
fn fetch(current: Version) -> UpdateEvent {
    match latest_tag() {
        Ok(Some(tag)) => match tag_in(&tag, current) {
            Ok(event) => event,
            Err(err) => UpdateEvent::Failed(err),
        },
        Ok(None) => UpdateEvent::UpToDate,
        Err(err) => UpdateEvent::Failed(err),
    }
}

/// Compares one tag against the running version.
///
/// Split out so the decision is testable without a network.
fn tag_in(tag: &str, current: Version) -> Result<UpdateEvent, String> {
    let version = Version::parse(tag).ok_or_else(|| format!("unparsable release tag: {tag}"))?;
    if version.is_newer_than(&current) {
        Ok(UpdateEvent::Available {
            tag: tag.to_owned(),
            version,
        })
    } else {
        Ok(UpdateEvent::UpToDate)
    }
}

/// Reads the tag out of the `/releases/latest` redirect.
///
/// Returns `Ok(None)` when the repository has no published release yet, which
/// is not an error worth logging as one.
fn latest_tag() -> Result<Option<String>, String> {
    // Two settings carry the whole design of this request:
    //
    // * `max_redirects(0)` — ureq follows 10 redirects by default, which would
    //   fetch the release page and throw the `Location` header away. The
    //   redirect itself is the answer.
    // * `TlsProvider::NativeTls` — ureq's default is Rustls, and the native
    //   provider is never selected automatically even when the feature is on.
    //   Leaving it out panics at request time ("provider is Rustls but feature
    //   is not enabled"), so the explicit provider is required, not stylistic.
    let agent = ureq::config::Config::builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .build()
        .new_agent();

    let response = match agent
        .get(LATEST_RELEASE)
        .header(
            "User-Agent",
            concat!("ClipForge/", env!("CARGO_PKG_VERSION")),
        )
        .config()
        .http_status_as_error(false)
        .build()
        .call()
    {
        Ok(response) => response,
        Err(err) => return Err(format!("request failed: {err}")),
    };

    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_redirection() {
        return Err(format!("unexpected status {status}"));
    }

    response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .and_then(tag_from_location)
        .map(|tag| Some(tag.to_owned()))
        .ok_or_else(|| "redirect had no usable release tag".to_owned())
}

/// `https://github.com/o/r/releases/tag/v0.1.3[?...]` -> `v0.1.3`.
fn tag_from_location(location: &str) -> Option<&str> {
    let (_, tag) = location.split_once("/releases/tag/")?;
    let tag = tag.split(['?', '#']).next().unwrap_or(tag);
    (!tag.is_empty()).then_some(tag)
}

/// Drains the channel; returns the first event once it arrives.
pub fn poll(rx: &Receiver<UpdateEvent>) -> Option<UpdateEvent> {
    rx.try_recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> Version {
        Version::parse("0.1.3").unwrap()
    }

    #[test]
    fn newer_tag_is_reported() {
        match tag_in("v0.1.4", current()) {
            Ok(UpdateEvent::Available { tag, version }) => {
                assert_eq!(tag, "v0.1.4");
                assert_eq!(version, Version::parse("0.1.4").unwrap());
            }
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn same_version_is_up_to_date() {
        assert_eq!(tag_in("v0.1.3", current()), Ok(UpdateEvent::UpToDate));
    }

    #[test]
    fn older_tag_is_up_to_date() {
        assert_eq!(tag_in("v0.1.0", current()), Ok(UpdateEvent::UpToDate));
    }

    #[test]
    fn unparsable_tag_reports_a_failure() {
        assert!(matches!(
            tag_in("nightly", current()),
            Err(message) if message.contains("tag")
        ));
    }

    #[test]
    fn extracts_the_tag_from_a_github_redirect() {
        assert_eq!(
            tag_from_location("https://github.com/TonySen1/clipforge/releases/tag/v0.1.3"),
            Some("v0.1.3")
        );
    }

    #[test]
    fn redirect_tag_ignores_query_and_fragment() {
        assert_eq!(
            tag_from_location("https://github.com/o/r/releases/tag/v0.2.0?expanded=true#notes"),
            Some("v0.2.0")
        );
    }

    #[test]
    fn a_redirect_without_a_tag_is_rejected() {
        assert_eq!(
            tag_from_location("https://github.com/TonySen1/clipforge/releases"),
            None
        );
        assert_eq!(
            tag_from_location("https://github.com/o/r/releases/tag/"),
            None
        );
    }
}
