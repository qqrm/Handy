//! A one-shot address for dictation output, captured before transcription.
//!
//! This module never changes foreground focus, injects global keys, or touches
//! the clipboard. Unsupported/changed targets are errors so the caller can keep
//! the entire transcription in the normal clipboard instead.

use std::fmt;

#[cfg(any(windows, test))]
mod text;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod write_request;

#[cfg(windows)]
pub use windows::{TargetCapture, TargetSnapshot};

/// The target could not safely accept the captured dictation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetError {
    Unavailable,
    Unsupported,
    Protected,
    Changed,
    Busy,
    /// A write was attempted, but its result could not be verified. Never retry
    /// automatically: the target might already contain some or all of the text.
    Uncertain,
    AlreadyUsed,
    InvalidText,
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "The original input field is no longer available",
            Self::Unsupported => "The input field does not support addressed insertion",
            Self::Protected => "The original input field is protected or read-only",
            Self::Changed => "The original input field or document changed",
            Self::Busy => "The original application did not respond in time",
            Self::Uncertain => "The original application did not confirm the insertion",
            Self::AlreadyUsed => "This dictation target has already been used",
            Self::InvalidText => "The text cannot be inserted into the original input field",
        })
    }
}

impl std::error::Error for TargetError {}

/// Returned only after reading back the expected value from the exact target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub verified: bool,
}

/// Addressed delivery is currently implemented for Windows. Other platforms
/// keep their existing output path in the caller.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct TargetSnapshot;

#[cfg(not(windows))]
impl TargetSnapshot {
    pub fn capture() -> Result<Self, TargetError> {
        Err(TargetError::Unsupported)
    }

    pub fn insert(&mut self, _text: &str) -> Result<DeliveryReceipt, TargetError> {
        Err(TargetError::Unsupported)
    }

    pub fn insert_if_active(
        &mut self,
        _text: &str,
        _active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<DeliveryReceipt, TargetError> {
        Err(TargetError::Unsupported)
    }

    pub fn is_focused_target(&self) -> Result<bool, TargetError> {
        Ok(false)
    }
}
