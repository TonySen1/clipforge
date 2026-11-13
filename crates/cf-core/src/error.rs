use std::path::PathBuf;

/// Errors originating in the core layer.
///
/// Variants carry structured context only; the UI maps them to localized
/// messages and may show the attached detail in an expandable section.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("configuration directory is unavailable")]
    NoConfigDir,

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    // --- FFmpeg discovery -------------------------------------------------
    #[error("FFmpeg was not found in any searched location")]
    FfmpegNotFound {
        attempts: Vec<(
            PathBuf,
            crate::media::ffmpeg::DiscoverySource,
            Box<CoreError>,
        )>,
    },

    #[error("FFmpeg binary is missing at {path}")]
    FfmpegMissing { path: PathBuf },

    #[error("FFmpeg at {path} is version {found:?}, but {required}+ is required")]
    FfmpegTooOld {
        path: PathBuf,
        found: Option<u32>,
        required: u32,
    },

    #[error("failed to start {path}: {source}")]
    FfmpegSpawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{program} exited with {code:?}: {stderr}")]
    FfmpegFailed {
        program: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("{program} produced unreadable output: {detail}")]
    FfmpegBadOutput { program: String, detail: String },

    // --- Media probing ----------------------------------------------------
    #[error("no video stream found in {path}")]
    NoVideoStream { path: PathBuf },

    #[error("media duration is unknown for {path}")]
    UnknownDuration { path: PathBuf },
}

impl CoreError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// One-line summary for the status bar. The UI localizes the prefix and
    /// shows the rest verbatim, since it is technical detail.
    pub fn detail(&self) -> String {
        match self {
            Self::FfmpegNotFound { attempts, .. } => attempts
                .iter()
                .map(|(path, source, err)| {
                    format!("{} ({}): {}", path.display(), source.label(), err.detail())
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => self.to_string(),
        }
    }
}

impl CoreError {
    pub fn ffmpeg_not_found(
        attempts: Vec<(PathBuf, crate::media::ffmpeg::DiscoverySource, CoreError)>,
    ) -> Self {
        Self::FfmpegNotFound {
            attempts: attempts
                .into_iter()
                .map(|(path, source, err)| (path, source, Box::new(err)))
                .collect(),
        }
    }

    pub fn not_found_attempts(
        &self,
    ) -> Option<
        &[(
            PathBuf,
            crate::media::ffmpeg::DiscoverySource,
            Box<CoreError>,
        )],
    > {
        match self {
            Self::FfmpegNotFound { attempts } => Some(attempts),
            _ => None,
        }
    }
}
