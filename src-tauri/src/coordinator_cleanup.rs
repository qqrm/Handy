//! Completion gate shared by the coordinator and its deterministic tests.
//! A completed Stop cannot release a still-running cancellation's cleanup.
#[derive(Default)]
pub(crate) struct CleanupFence {
    processing_pending: bool,
    cancellation_pending: bool,
}

impl CleanupFence {
    pub(crate) fn start_processing(&mut self) {
        self.processing_pending = true;
    }

    /// Repeated cancel requests share the same cleanup worker.
    pub(crate) fn start_cancellation(&mut self) -> bool {
        !std::mem::replace(&mut self.cancellation_pending, true)
    }

    /// Returns true only when an expected completion releases the last owner.
    pub(crate) fn finish_processing(&mut self) -> bool {
        if !std::mem::replace(&mut self.processing_pending, false) {
            return false;
        }
        !self.cancellation_pending
    }

    pub(crate) fn finish_cancellation(&mut self) -> bool {
        if !std::mem::replace(&mut self.cancellation_pending, false) {
            return false;
        }
        !self.processing_pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_completion_waits_for_full_cancellation_cleanup() {
        let mut fence = CleanupFence::default();
        fence.start_processing();
        assert!(fence.start_cancellation());
        assert!(!fence.finish_processing());
        assert!(fence.finish_cancellation());
    }

    #[test]
    fn cancellation_completion_waits_for_capture_and_processing() {
        let mut fence = CleanupFence::default();
        fence.start_processing();
        assert!(fence.start_cancellation());
        assert!(!fence.finish_cancellation());
        assert!(fence.finish_processing());
    }

    #[test]
    fn repeated_cancel_coalesces_without_an_extra_worker_or_completion() {
        let mut fence = CleanupFence::default();
        assert!(fence.start_cancellation());
        for _ in 0..100 {
            assert!(!fence.start_cancellation());
        }
        assert!(fence.finish_cancellation());
        assert!(!fence.finish_cancellation());
    }

    #[test]
    fn unexpected_completions_cannot_change_a_new_recordings_stage() {
        let mut fence = CleanupFence::default();
        assert!(!fence.finish_processing());
        assert!(!fence.finish_cancellation());
    }
}
