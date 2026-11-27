//! Masked screenshot artifacts: where the broker leaves a page capture for the
//! Discord gateway to read (ADR 0008).
//!
//! The engine captures and masks screenshots itself
//! ([`crate::ports::CamofoxPort::capture_masked_result`]) — masking is fused to
//! capture, so an image that reaches this store has already had the payment
//! fields cleared. The gateway then reads the file by payment id instead of
//! receiving pixels over a port, which keeps the two crates coupled by a path
//! and nothing else.
//!
//! # Why a directory and not a new port method
//!
//! The gateway already has to hand the human *something* addressable: a Discord
//! attachment upload needs a file to upload. Writing one file per
//! `(payment_id, kind)` inside a broker-owned directory is the smallest seam
//! that gives the gateway a path, needs no new inter-crate type, and makes the
//! "what did the human actually see" question answerable during an incident.
//!
//! # Card data
//!
//! Only masked captures are stored. A file written here has had the payment
//! fields cleared before the screenshot was taken (US-10), so the store itself
//! never handles a card value — it only writes bytes it is handed.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::payment::PaymentId;

/// Environment variable naming the artifacts directory (ADR 0008).
///
/// Deployment sets it; the server binary resolves it the same way it resolves
/// `TILL_DB` and `TILL_AGE_IDENTITY`.
pub const ARTIFACTS_DIR_ENV_VAR: &str = "TILL_ARTIFACTS_DIR";

/// Used when [`ARTIFACTS_DIR_ENV_VAR`] is unset: for local development.
pub const DEFAULT_ARTIFACTS_DIR: &str = "artifacts";

/// Which capture of a payment an artifact is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// The checkout page shown with the approval request (US-6). No card value
    /// has been typed at that point; stored here so the gateway has one seam.
    Approval,
    /// The post-submit page the human classifies (US-10), captured after the
    /// payment fields were cleared.
    Result,
}

impl ArtifactKind {
    /// The stable file-name component and directory name for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Result => "result",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The broker-managed directory of masked screenshots, keyed by payment id.
#[derive(Debug, Clone)]
pub struct ScreenshotStore {
    dir: PathBuf,
}

impl ScreenshotStore {
    /// Open (creating if absent) the artifacts directory.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the directory cannot be created.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory artifacts are written to.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the artifact for `(payment_id, kind)` lives.
    ///
    /// This is the whole gateway contract: given a payment id and a kind, a
    /// reader asks for this path (ADR 0008).
    #[must_use]
    pub fn path(&self, payment_id: PaymentId, kind: ArtifactKind) -> PathBuf {
        self.dir.join(format!("{payment_id}-{}.png", kind.as_str()))
    }

    /// Write one masked capture and return its path.
    ///
    /// The bytes land via a temporary file in the same directory and a rename,
    /// so a reader can never observe a half-written PNG. On unix the file is
    /// owner-only: it is a page capture, and least privilege costs nothing here.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the file cannot be written or renamed into place.
    pub fn store(
        &self,
        payment_id: PaymentId,
        kind: ArtifactKind,
        png: &[u8],
    ) -> io::Result<PathBuf> {
        let path = self.path(payment_id, kind);
        let temporary = path.with_extension("png.part");

        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(png)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temporary, &path)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArtifactKind, ScreenshotStore};
    use crate::payment::PaymentId;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("till-artifacts-{tag}-{}-{seq}", std::process::id()))
    }

    fn sample_png() -> Vec<u8> {
        // A PNG signature plus filler: the store must be byte-transparent.
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    #[test]
    fn store_writes_atomically_with_owner_only_permissions() -> std::io::Result<()> {
        let dir = temp_dir("store");
        let store = ScreenshotStore::open(&dir)?;
        let payment_id = PaymentId::new();
        let png = sample_png();

        let path = store.store(payment_id, ArtifactKind::Result, &png)?;

        assert_eq!(
            std::fs::read(&path)?,
            png,
            "the artifact is byte-transparent"
        );
        assert!(
            !path.with_extension("png.part").exists(),
            "no partial file is left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a page capture is owner-only");
        }

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn path_is_keyed_by_payment_id_and_kind() {
        let store = ScreenshotStore {
            dir: std::path::PathBuf::from("/artifacts"),
        };
        let payment_id = PaymentId::new();

        let result = store.path(payment_id, ArtifactKind::Result);
        let approval = store.path(payment_id, ArtifactKind::Approval);

        assert_eq!(
            result.file_name().and_then(|name| name.to_str()),
            Some(format!("{payment_id}-result.png").as_str()),
            "the gateway reads by payment id and kind"
        );
        assert_ne!(result, approval, "the two kinds never collide");
    }
}
