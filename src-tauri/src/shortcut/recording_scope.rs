//! Generation token shared by registration workers and recording-only callbacks.

use std::sync::atomic::{AtomicU64, Ordering};

const ACTIVE: u64 = 1;
const SUSPENDED: u64 = 2;
const FLAGS: u64 = ACTIVE | SUSPENDED;

#[derive(Default)]
pub(super) struct RecordingScope(AtomicU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Snapshot(u64);

impl Snapshot {
    pub(super) fn token(self) -> Option<u64> {
        ((self.0 & FLAGS) == ACTIVE).then_some(self.0)
    }
}

impl RecordingScope {
    pub(super) fn snapshot(&self) -> Snapshot {
        Snapshot(self.0.load(Ordering::SeqCst))
    }

    pub(super) fn accepts(&self, token: u64) -> bool {
        self.snapshot().token() == Some(token)
    }

    pub(super) fn start(&self) {
        self.update(Some(true), None);
    }

    pub(super) fn stop(&self) {
        self.update(Some(false), None);
    }

    pub(super) fn suspend(&self) {
        self.update(None, Some(true));
    }

    pub(super) fn resume(&self) {
        self.update(None, Some(false));
    }

    pub(super) fn refresh(&self) {
        self.update(None, None);
    }

    fn update(&self, active: Option<bool>, suspended: Option<bool>) {
        // Flags and generation change together; callbacks cannot observe an
        // active flag from one session with the token from another.
        let mut old = self.0.load(Ordering::SeqCst);
        loop {
            let mut flags = old & FLAGS;
            if let Some(active) = active {
                flags = (flags & !ACTIVE) | if active { ACTIVE } else { 0 };
            }
            if let Some(suspended) = suspended {
                flags = (flags & !SUSPENDED) | if suspended { SUSPENDED } else { 0 };
            }
            let next = (old & !FLAGS).wrapping_add(4) | flags;
            match self
                .0
                .compare_exchange_weak(old, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => old = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_registration_cannot_survive_stop() {
        let scope = RecordingScope::default();
        scope.start();
        let pending_registration = scope.snapshot().token().unwrap();
        scope.stop();
        // A worker may finish registering after Stop. Its callback is already
        // inert and its stale snapshot forces immediate reconciliation.
        assert!(!scope.accepts(pending_registration));
        assert!(scope.snapshot().token().is_none());
    }

    #[test]
    fn old_callback_cannot_stop_a_new_recording() {
        let scope = RecordingScope::default();
        scope.start();
        let old = scope.snapshot().token().unwrap();
        scope.stop();
        scope.start();
        let current = scope.snapshot().token().unwrap();
        assert!(!scope.accepts(old));
        assert!(scope.accepts(current));
    }

    #[test]
    fn shortcut_capture_suspends_and_invalidates_recording_actions() {
        let scope = RecordingScope::default();
        scope.start();
        let old = scope.snapshot().token().unwrap();
        scope.suspend();
        assert!(!scope.accepts(old));
        scope.resume();
        assert!(!scope.accepts(old));
        assert!(scope.snapshot().token().is_some());
        scope.stop();
        scope.suspend();
        scope.resume();
        assert!(scope.snapshot().token().is_none());
    }

    #[test]
    fn binding_or_backend_edit_invalidates_previous_callback() {
        let scope = RecordingScope::default();
        scope.start();
        let old = scope.snapshot().token().unwrap();
        scope.refresh();
        assert!(!scope.accepts(old));
        assert!(scope.snapshot().token().is_some());
    }
}
