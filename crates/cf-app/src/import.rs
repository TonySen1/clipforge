//! Background import worker.
//!
//! Probing a file is two subprocess round trips (ffprobe metadata, then the
//! keyframe scan), which on a long file can take seconds. Both run on a
//! dedicated thread and report back through a channel; the UI thread only ever
//! polls.

use cf_core::domain::{MediaSource, SourceId};
use cf_core::media::{ffmpeg, keyframes, probe, thumbs, DiscoverySource, FfmpegInstall};
use cf_core::CoreError;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// Messages sent from the worker back to the UI.
#[derive(Debug)]
pub enum ImportEvent {
    /// The FFmpeg install was located (or failed to be located).
    Engine(Result<EngineInfo, Arc<CoreError>>),
    Started {
        path: PathBuf,
    },
    /// Metadata is available; the keyframe scan may still be running.
    Metadata {
        source: MediaSource,
        keyframe_warning: Option<String>,
    },
    /// The scrub strip finished decoding. Sent separately so the source shows
    /// up in the list immediately instead of waiting for the extra pass.
    Thumbs {
        source_id: cf_core::SourceId,
        strip: cf_core::media::KeyframeStrip,
    },
    Failed {
        path: PathBuf,
        error: Arc<CoreError>,
    },
}

/// What the UI needs to know about the FFmpeg installation.
#[derive(Clone, Debug)]
pub struct EngineInfo {
    pub install: Arc<FfmpegInstall>,
    pub source: DiscoverySource,
}

impl EngineInfo {
    pub fn version_label(&self) -> String {
        match self.install.version.major {
            Some(major) => format!("{major}.x"),
            None => "git".to_owned(),
        }
    }
}

/// Owns the worker thread and the receiving end of its channel.
pub struct ImportWorker {
    tx: Sender<Job>,
    events: Receiver<ImportEvent>,
}

enum Job {
    Discover(Option<PathBuf>),
    Import { path: PathBuf, next_id: u64 },
    Shutdown,
}

impl ImportWorker {
    pub fn spawn(ctx: egui::Context) -> Self {
        let (tx, job_rx) = mpsc::channel::<Job>();
        let (event_tx, events) = mpsc::channel::<ImportEvent>();

        std::thread::Builder::new()
            .name("clipforge-import".to_owned())
            .spawn(move || {
                let mut engine: Option<Arc<FfmpegInstall>> = None;

                while let Ok(job) = job_rx.recv() {
                    match job {
                        Job::Shutdown => break,
                        Job::Discover(explicit) => {
                            let result = ffmpeg::discover(explicit.as_deref())
                                .map(|(install, source)| EngineInfo {
                                    install: Arc::new(install),
                                    source,
                                })
                                .map_err(Arc::new);

                            if let Ok(info) = &result {
                                engine = Some(info.install.clone());
                            }
                            let _ = event_tx.send(ImportEvent::Engine(result));
                            ctx.request_repaint();
                        }
                        Job::Import { path, next_id } => {
                            let _ = event_tx.send(ImportEvent::Started { path: path.clone() });
                            ctx.request_repaint();

                            let Some(install) = engine.clone() else {
                                let error = Arc::new(CoreError::ffmpeg_not_found(Vec::new()));
                                let _ = event_tx.send(ImportEvent::Failed { path, error });
                                ctx.request_repaint();
                                continue;
                            };

                            let info = match probe::probe(&install, &path) {
                                Ok(info) => info,
                                Err(error) => {
                                    let _ = event_tx.send(ImportEvent::Failed {
                                        path,
                                        error: Arc::new(error),
                                    });
                                    ctx.request_repaint();
                                    continue;
                                }
                            };

                            // A failed keyframe scan must not lose the source:
                            // preview still works, only lossless export degrades.
                            let (source, warning) = match keyframes::probe(&install, &path) {
                                Ok(index) if !index.is_empty() => {
                                    (MediaSource::new(SourceId(next_id), info, index), None)
                                }
                                Ok(_) => (
                                    MediaSource::with_degraded_keyframes(SourceId(next_id), info),
                                    Some("no keyframes found".to_owned()),
                                ),
                                Err(err) => (
                                    MediaSource::with_degraded_keyframes(SourceId(next_id), info),
                                    Some(err.detail()),
                                ),
                            };
                            // Cheap `Arc` clones so the strip can be built after
                            // the source itself has been handed to the UI.
                            let strip_info = source.info.clone();
                            let _ = event_tx.send(ImportEvent::Metadata {
                                source,
                                keyframe_warning: warning,
                            });
                            ctx.request_repaint();

                            // The strip is a second pass over the file. It is
                            // sent separately so the source appears at once; a
                            // failure here only costs the instant scrub preview.
                            let strip = thumbs::build(
                                &install,
                                &path,
                                &strip_info,
                                thumbs::STRIP_MAX_EDGE,
                                thumbs::STRIP_BUDGET_BYTES,
                            );
                            match strip {
                                Ok(strip) if !strip.is_empty() => {
                                    let _ = event_tx.send(ImportEvent::Thumbs {
                                        source_id: SourceId(next_id),
                                        strip,
                                    });
                                    ctx.request_repaint();
                                }
                                Ok(_) => {}
                                Err(err) => log::warn!(
                                    "scrub strip for {} failed: {}",
                                    path.display(),
                                    err.detail()
                                ),
                            }
                        }
                    }
                }
            })
            .expect("spawning the import worker");

        Self { tx, events }
    }

    pub fn discover(&self, explicit: Option<PathBuf>) {
        let _ = self.tx.send(Job::Discover(explicit));
    }

    pub fn import(&self, path: PathBuf, next_id: u64) {
        let _ = self.tx.send(Job::Import { path, next_id });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Job::Shutdown);
    }

    pub fn try_recv(&self) -> Option<ImportEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for ImportWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Job::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_label_prefers_major_release() {
        let info = EngineInfo {
            install: Arc::new(FfmpegInstall {
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: PathBuf::from("ffprobe"),
                version: cf_core::media::FfmpegVersion {
                    banner: String::new(),
                    major: Some(7),
                    git_build: false,
                    gpl: true,
                    encoders: Default::default(),
                },
            }),
            source: DiscoverySource::Path,
        };
        assert_eq!(info.version_label(), "7.x");
    }

    #[test]
    fn version_label_marks_git_builds() {
        let info = EngineInfo {
            install: Arc::new(FfmpegInstall {
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: PathBuf::from("ffprobe"),
                version: cf_core::media::FfmpegVersion {
                    banner: String::new(),
                    major: None,
                    git_build: true,
                    gpl: true,
                    encoders: Default::default(),
                },
            }),
            source: DiscoverySource::Path,
        };
        assert_eq!(info.version_label(), "git");
    }
}
