//! Clock and cancellation policy, independent of the desktop/audio runtime.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A recording owns one lease. Cancel and the actual popup/audio side effects
/// use the same lock, so queued work cannot begin after Stop has invalidated it.
#[derive(Clone)]
pub(crate) struct ReminderLease(Arc<(Mutex<LeaseState>, Condvar)>);

struct LeaseState {
    active: bool,
    cancel: Vec<Box<dyn FnOnce() + Send>>,
}

impl ReminderLease {
    pub(crate) fn new() -> Self {
        Self(Arc::new((
            Mutex::new(LeaseState {
                active: true,
                cancel: Vec::new(),
            }),
            Condvar::new(),
        )))
    }

    pub(crate) fn cancel(&self) {
        let (state, wake) = &*self.0;
        // Cleanup remains necessary even if an earlier side effect panicked.
        let mut state = match state.lock() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        };
        state.active = false;
        for cancel in state.cancel.drain(..) {
            cancel();
        }
        wake.notify_all();
    }

    pub(crate) fn with_active<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let state = self.0 .0.lock().ok()?;
        state.active.then(action)
    }

    /// Register cleanup before starting an effect. Registering after Stop runs
    /// the cleanup immediately; `with_active` will then reject the effect.
    pub(crate) fn on_cancel(&self, cancel: impl FnOnce() + Send + 'static) {
        match self.0 .0.lock() {
            Ok(mut state) if state.active => state.cancel.push(Box::new(cancel)),
            _ => cancel(),
        }
    }

    /// Sleep without polling, waking immediately on Stop/Cancel. Spurious
    /// wakeups do not advance the schedule.
    pub(crate) fn wait(&self, duration: Duration) -> bool {
        let (state, wake) = &*self.0;
        let Ok(state) = state.lock() else {
            return false;
        };
        match wake.wait_timeout_while(state, duration, |state| state.active) {
            Ok((state, _)) => state.active,
            Err(_) => false,
        }
    }
}

/// First reminder after X, then wait 2X, 4X, 8X ... . At X=1 minute the
/// elapsed times are 1, 3, 7, 15 minutes. A delayed timer emits once and starts
/// its next interval from now instead of flooding the user with catch-up.
pub(crate) struct ReminderSchedule {
    next: Duration,
    interval: Duration,
}

impl ReminderSchedule {
    pub(crate) fn new(first: Duration) -> Self {
        let first = first.max(Duration::from_secs(1));
        Self {
            next: first,
            interval: first,
        }
    }

    pub(crate) fn wait_for(&self, elapsed: Duration) -> Duration {
        self.next.saturating_sub(elapsed)
    }

    pub(crate) fn poll(&mut self, elapsed: Duration) -> bool {
        if elapsed < self.next {
            return false;
        }
        self.interval = self.interval.saturating_mul(2);
        self.next = elapsed.saturating_add(self.interval);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn reminder_intervals_double_from_the_first_notification() {
        let mut schedule = ReminderSchedule::new(Duration::from_secs(60));
        for minute in [1, 3, 7, 15, 31] {
            let due = Duration::from_secs(minute * 60);
            assert!(!schedule.poll(due - Duration::from_millis(1)));
            assert!(schedule.poll(due));
            assert!(!schedule.poll(due));
        }
    }

    #[test]
    fn custom_interval_is_used_for_the_entire_progression() {
        let mut schedule = ReminderSchedule::new(Duration::from_secs(120));
        assert_eq!(schedule.wait_for(Duration::ZERO), Duration::from_secs(120));
        assert!(!schedule.poll(Duration::from_secs(60)));
        assert!(schedule.poll(Duration::from_secs(120)));
        assert_eq!(
            schedule.wait_for(Duration::from_secs(120)),
            Duration::from_secs(240)
        );
        assert!(schedule.poll(Duration::from_secs(360)));
    }

    #[test]
    fn timer_delay_does_not_create_a_catch_up_burst() {
        let mut schedule = ReminderSchedule::new(Duration::from_secs(60));
        assert!(schedule.poll(Duration::from_secs(600)));
        assert!(!schedule.poll(Duration::from_secs(600)));
        assert_eq!(
            schedule.wait_for(Duration::from_secs(600)),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn interval_arithmetic_saturates_instead_of_wrapping() {
        let mut schedule = ReminderSchedule::new(Duration::MAX);
        assert!(!schedule.poll(Duration::from_secs(60)));
        assert!(schedule.poll(Duration::MAX));
        assert_eq!(schedule.wait_for(Duration::ZERO), Duration::MAX);
    }

    #[test]
    fn cancellation_rejects_queued_visual_and_audio_effects() {
        let old = ReminderLease::new();
        let queued = old.clone();
        old.cancel();
        let next = ReminderLease::new();
        assert_eq!(queued.with_active(|| "late popup"), None);
        assert_eq!(queued.with_active(|| "late sound"), None);
        assert_eq!(next.with_active(|| "new recording"), Some("new recording"));
    }

    #[test]
    fn cancellation_stops_an_already_started_effect_once() {
        let lease = ReminderLease::new();
        let stopped = Arc::new(AtomicUsize::new(0));
        let cleanup = stopped.clone();
        lease.on_cancel(move || {
            cleanup.fetch_add(1, Ordering::SeqCst);
        });
        assert!(lease.with_active(|| ()).is_some());
        lease.cancel();
        lease.cancel();
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        let cleanup = stopped.clone();
        lease.on_cancel(move || {
            cleanup.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(stopped.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancellation_wakes_a_sleeping_timer_without_waiting_for_deadline() {
        let lease = ReminderLease::new();
        let sleeping = lease.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready_tx.send(()).expect("ready receiver");
            sleeping.wait(Duration::from_secs(60 * 60))
        });
        ready_rx.recv().expect("worker started");
        lease.cancel();
        assert!(!worker.join().expect("timer worker"));
    }
}
