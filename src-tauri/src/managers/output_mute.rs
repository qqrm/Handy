//! A recording owns only the output mute that it actually changed.
//!
//! The output identity is captured before reading or changing mute. Restoration
//! uses that identity even if the system default changes during the recording.
//! An unreadable state is never treated as "unmuted".

#[cfg(target_os = "linux")]
#[path = "output_mute_linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "output_mute_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "output_mute_windows.rs"]
mod platform;

/// A successful, reversible change to one output. Keep the lease after a failed
/// restoration so cleanup can retry; a successful restoration is idempotent.
pub(super) trait MutedOutput: Send {
    fn restore(&mut self) -> Result<(), String>;
}

/// The adapters resolve the default only here. Every later operation accepts
/// the captured identity explicitly and must never resolve the default again.
trait OutputBackend: Send + 'static {
    type Target: Send + 'static;

    fn default_target(&self) -> Result<Self::Target, String>;
    fn is_muted(&self, target: &Self::Target) -> Result<bool, String>;
    fn set_muted(&self, target: &Self::Target, muted: bool) -> Result<(), String>;
}

struct OutputMute<B: OutputBackend> {
    backend: B,
    target: B::Target,
    active: bool,
}

impl<B: OutputBackend> MutedOutput for OutputMute<B> {
    fn restore(&mut self) -> Result<(), String> {
        if self.active {
            self.backend.set_muted(&self.target, false)?;
            self.active = false;
        }
        Ok(())
    }
}

fn mute_with_backend<B: OutputBackend>(backend: B) -> Result<Option<Box<dyn MutedOutput>>, String> {
    let target = backend.default_target()?;
    if backend.is_muted(&target)? {
        return Ok(None);
    }
    backend.set_muted(&target, true)?;
    Ok(Some(Box::new(OutputMute {
        backend,
        target,
        active: true,
    })))
}

/// Mutes the current default output, returning ownership only when it was
/// successfully changed from an observed unmuted state. No global unmute is
/// attempted when discovery, state reading, or the mute command fails.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub(super) fn mute_default_output() -> Result<Option<Box<dyn MutedOutput>>, String> {
    mute_with_backend(platform::backend())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(super) fn mute_default_output() -> Result<Option<Box<dyn MutedOutput>>, String> {
    Err("Muting the default output is not supported on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeState {
        default: u32,
        muted: HashMap<u32, bool>,
        reads_fail: bool,
        writes_fail: bool,
        writes: Vec<(u32, bool)>,
        // Simulates the default changing immediately after discovery.
        change_after_discovery: Option<u32>,
    }

    #[derive(Clone)]
    struct FakeBackend(Arc<Mutex<FakeState>>);

    impl FakeBackend {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(FakeState {
                default: 1,
                muted: HashMap::from([(1, false), (2, true)]),
                ..FakeState::default()
            })))
        }
    }

    impl OutputBackend for FakeBackend {
        type Target = u32;

        fn default_target(&self) -> Result<Self::Target, String> {
            let mut state = self.0.lock().unwrap();
            let target = state.default;
            if let Some(replacement) = state.change_after_discovery.take() {
                state.default = replacement;
            }
            Ok(target)
        }

        fn is_muted(&self, target: &Self::Target) -> Result<bool, String> {
            let state = self.0.lock().unwrap();
            if state.reads_fail {
                return Err("Cannot read mute".into());
            }
            state
                .muted
                .get(target)
                .copied()
                .ok_or("Output missing".into())
        }

        fn set_muted(&self, target: &Self::Target, muted: bool) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            state.writes.push((*target, muted));
            if state.writes_fail {
                return Err("Cannot set mute".into());
            }
            *state.muted.get_mut(target).ok_or("Output missing")? = muted;
            Ok(())
        }
    }

    #[test]
    fn restore_uses_original_output_after_default_changes() {
        let backend = FakeBackend::new();
        let mut lease = mute_with_backend(backend.clone()).unwrap().unwrap();
        backend.0.lock().unwrap().default = 2;
        lease.restore().unwrap();

        let state = backend.0.lock().unwrap();
        assert!(!state.muted[&1], "The output we muted must be restored");
        assert!(state.muted[&2], "The new default must remain untouched");
        assert_eq!(state.writes, [(1, true), (1, false)]);
    }

    #[test]
    fn state_read_and_mute_use_the_same_captured_output() {
        let backend = FakeBackend::new();
        backend.0.lock().unwrap().change_after_discovery = Some(2);
        let mut lease = mute_with_backend(backend.clone()).unwrap().unwrap();
        assert_eq!(backend.0.lock().unwrap().writes, [(1, true)]);
        lease.restore().unwrap();
        assert!(backend.0.lock().unwrap().muted[&2]);
    }

    #[test]
    fn already_muted_output_is_not_owned() {
        let backend = FakeBackend::new();
        backend.0.lock().unwrap().muted.insert(1, true);
        assert!(mute_with_backend(backend.clone()).unwrap().is_none());
        assert!(backend.0.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn unreadable_mute_state_is_not_changed() {
        let backend = FakeBackend::new();
        backend.0.lock().unwrap().reads_fail = true;
        assert!(mute_with_backend(backend.clone()).is_err());
        assert!(backend.0.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn failed_mute_does_not_create_a_restore_lease() {
        let backend = FakeBackend::new();
        backend.0.lock().unwrap().writes_fail = true;
        assert!(mute_with_backend(backend.clone()).is_err());
        assert_eq!(backend.0.lock().unwrap().writes, [(1, true)]);
    }

    #[test]
    fn failed_restore_retains_ownership_for_retry() {
        let backend = FakeBackend::new();
        let mut lease = mute_with_backend(backend.clone()).unwrap().unwrap();
        backend.0.lock().unwrap().writes_fail = true;
        assert!(lease.restore().is_err());
        {
            let mut state = backend.0.lock().unwrap();
            state.writes_fail = false;
            state.default = 2;
        }
        lease.restore().unwrap();
        assert_eq!(
            backend.0.lock().unwrap().writes,
            [(1, true), (1, false), (1, false)]
        );
        assert!(!backend.0.lock().unwrap().muted[&1]);
        assert!(backend.0.lock().unwrap().muted[&2]);
    }

    #[test]
    fn missing_original_output_does_not_unmute_the_new_default() {
        let backend = FakeBackend::new();
        let mut lease = mute_with_backend(backend.clone()).unwrap().unwrap();
        {
            let mut state = backend.0.lock().unwrap();
            state.muted.remove(&1);
            state.default = 2;
        }
        assert!(lease.restore().is_err());
        assert!(backend.0.lock().unwrap().muted[&2]);
    }

    #[test]
    fn repeated_restore_does_not_touch_output_again() {
        let backend = FakeBackend::new();
        let mut lease = mute_with_backend(backend.clone()).unwrap().unwrap();
        lease.restore().unwrap();
        // A later manual mute is no longer ours to undo.
        backend.0.lock().unwrap().muted.insert(1, true);
        lease.restore().unwrap();
        assert!(backend.0.lock().unwrap().muted[&1]);
        assert_eq!(backend.0.lock().unwrap().writes, [(1, true), (1, false)]);
    }
}
