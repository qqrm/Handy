use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Default)]
struct FakeOutput {
    muted: AtomicBool,
    paused: AtomicBool,
    mute_error: AtomicBool,
    restore_error: AtomicBool,
    mute_calls: AtomicUsize,
    restore_calls: AtomicUsize,
    events: Mutex<Vec<&'static str>>,
    mute_gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl FakeOutput {
    fn event(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }

    fn protected(&self) -> (bool, bool) {
        (
            self.muted.load(Ordering::SeqCst),
            self.paused.load(Ordering::SeqCst),
        )
    }
}

struct FakeLease(Arc<FakeOutput>);

impl MutedOutput for FakeLease {
    fn restore(&mut self) -> Result<(), String> {
        self.0.restore_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.restore_error.load(Ordering::SeqCst) {
            return Err("endpoint temporarily unavailable".into());
        }
        self.0.muted.store(false, Ordering::SeqCst);
        self.0.event("unmute");
        Ok(())
    }
}

struct FakeBackend(Arc<FakeOutput>);

impl OutputBackend for FakeBackend {
    fn begin(&self, _: u64) {}

    fn mute(&self) -> Result<Option<Box<dyn MutedOutput>>, String> {
        self.0.mute_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.0.mute_gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if self.0.mute_error.load(Ordering::SeqCst) {
            return Err("mute unsupported".into());
        }
        if self.0.muted.swap(true, Ordering::SeqCst) {
            return Ok(None);
        }
        self.0.event("mute");
        Ok(Some(Box::new(FakeLease(self.0.clone()))))
    }

    fn pause(&self, _: u64) {
        self.0.paused.store(true, Ordering::SeqCst);
        self.0.event("pause");
    }

    fn resume(&self) {
        if self.0.paused.swap(false, Ordering::SeqCst) {
            self.0.event("resume");
        }
    }

    fn drain(&self) {}
}

fn controller() -> (RecordingOutputController, Arc<FakeOutput>) {
    let output = Arc::new(FakeOutput::default());
    let controller = RecordingOutputController::with_backend(Arc::new(FakeBackend(output.clone())));
    (controller, output)
}

fn both() -> OutputOptions {
    OutputOptions {
        mute: true,
        pause: true,
    }
}

#[test]
fn all_four_setting_combinations_are_independent() {
    for mute in [false, true] {
        for pause in [false, true] {
            let (controller, output) = controller();
            controller.begin(1, OutputOptions { mute, pause });
            controller.ready(1);
            assert_eq!(output.protected(), (mute, pause));
            controller.stop_requested(1);
            assert_eq!(output.protected(), (mute, pause));
            controller.finish_after_capture(1, || {
                assert_eq!(output.protected(), (mute, pause));
            });
            assert_eq!(output.protected(), (false, false));
        }
    }
}

#[test]
fn cancel_restores_output_with_open_microphone() {
    let (controller, output) = controller();
    let microphone_open = AtomicBool::new(true);
    controller.begin(1, both());
    controller.ready(1);
    controller.stop_requested(1);
    controller.finish_after_capture(1, || {
        assert_eq!(output.protected(), (true, true));
        output.event("capture_stopped");
        // Always-on and lazy-close both leave the microphone open here.
    });
    assert!(microphone_open.load(Ordering::SeqCst));
    assert_eq!(output.protected(), (false, false));
    assert_eq!(output.restore_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *output.events.lock().unwrap(),
        ["mute", "pause", "capture_stopped", "unmute", "resume"]
    );
    assert_eq!(
        controller.finish_after_capture(1, || panic!("duplicate stop")),
        None
    );
    assert_eq!(output.restore_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn stop_keeps_output_protected_through_tail() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    controller.stop_requested(1);
    assert_eq!(output.protected(), (true, true));
    controller.finish_after_capture(1, || {
        output.event("tail_sample");
        assert_eq!(output.protected(), (true, true));
        output.event("capture_stopped");
        123
    });
    assert_eq!(
        *output.events.lock().unwrap(),
        [
            "mute",
            "pause",
            "tail_sample",
            "capture_stopped",
            "unmute",
            "resume"
        ]
    );
}

#[test]
fn cancel_during_tail_does_not_restore_before_capture_stops() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    controller.stop_requested(1);
    let (in_tail, entered) = mpsc::channel();
    let (cancel, cancelled) = mpsc::channel();
    let worker = controller.clone();
    let worker_output = output.clone();
    let thread = std::thread::spawn(move || {
        worker.finish_after_capture(1, || {
            in_tail.send(()).unwrap();
            cancelled.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(worker_output.protected(), (true, true));
            worker_output.event("capture_stopped");
        });
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    controller.stop_requested(1);
    assert_eq!(output.protected(), (true, true));
    assert_eq!(
        controller.finish_after_capture(1, || panic!("second capture owner")),
        None
    );
    cancel.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(output.protected(), (false, false));
}

#[test]
fn stale_readiness_cannot_mute_after_finish() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.stop_requested(1);
    controller.ready(1);
    assert_eq!(output.mute_calls.load(Ordering::SeqCst), 0);
    controller.finish_after_capture(1, || ());
    controller.ready(1);
    assert_eq!(output.mute_calls.load(Ordering::SeqCst), 0);
    assert_eq!(output.protected(), (false, false));
}

#[test]
fn finish_serializes_with_an_in_flight_mute() {
    let (controller, output) = controller();
    let (entered, in_mute) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *output.mute_gate.lock().unwrap() = Some((entered, released));
    controller.begin(1, both());
    let ready_controller = controller.clone();
    let ready = std::thread::spawn(move || ready_controller.ready(1));
    in_mute.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        controller.0.state.try_lock().is_err(),
        "The generation fence must remain locked through the mute side effect"
    );
    let finish_controller = controller.clone();
    let finish_output = output.clone();
    let finish = std::thread::spawn(move || {
        finish_controller.finish_after_capture(1, || {
            assert_eq!(finish_output.protected(), (true, true));
            finish_output.event("capture_stopped");
        });
    });
    release.send(()).unwrap();
    ready.join().unwrap();
    finish.join().unwrap();
    assert_eq!(output.protected(), (false, false));
    assert_eq!(
        *output.events.lock().unwrap(),
        ["mute", "pause", "capture_stopped", "unmute", "resume"]
    );
}

#[test]
fn stale_finish_does_not_restore_new_recording() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    controller.finish_after_capture(1, || ());
    controller.begin(2, both());
    controller.ready(2);
    controller.stop_requested(1);
    controller.ready(1);
    assert_eq!(
        controller.finish_after_capture(1, || panic!("stale capture stop")),
        None
    );
    assert_eq!(output.protected(), (true, true));
    controller.finish(1);
    assert_eq!(output.protected(), (true, true));
    controller.finish_after_capture(2, || ());
    assert_eq!(output.protected(), (false, false));
}

#[test]
fn duplicate_readiness_does_not_replace_original_mute_snapshot() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    controller.ready(1);
    assert_eq!(output.mute_calls.load(Ordering::SeqCst), 1);
    controller.finish_after_capture(1, || ());
    assert_eq!(output.protected(), (false, false));
}

#[test]
fn originally_muted_output_stays_muted() {
    let (controller, output) = controller();
    output.muted.store(true, Ordering::SeqCst);
    controller.begin(1, both());
    controller.ready(1);
    controller.finish_after_capture(1, || ());
    assert_eq!(output.protected(), (true, false));
    assert_eq!(output.restore_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn mute_failure_does_not_disable_pause() {
    let (controller, output) = controller();
    output.mute_error.store(true, Ordering::SeqCst);
    controller.begin(1, both());
    controller.ready(1);
    assert_eq!(output.protected(), (false, true));
    controller.finish_after_capture(1, || ());
    assert_eq!(output.restore_calls.load(Ordering::SeqCst), 0);
    assert_eq!(output.protected(), (false, false));
}

#[test]
fn failed_restore_retains_existing_mute_until_next_capture_finishes() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    output.restore_error.store(true, Ordering::SeqCst);
    controller.finish_after_capture(1, || ());
    assert_eq!(output.protected(), (true, false));
    controller.begin(2, both());
    assert_eq!(output.restore_calls.load(Ordering::SeqCst), 1);
    controller.ready(2);
    assert_eq!(output.mute_calls.load(Ordering::SeqCst), 2);
    assert_eq!(controller.0.state.lock().unwrap().muted.len(), 1);
    assert_eq!(output.protected(), (true, true));
    output.restore_error.store(false, Ordering::SeqCst);
    controller.finish_after_capture(2, || ());
    assert_eq!(output.protected(), (false, false));
}

// A separate device state for each default makes a retained A distinguishable
// from the healthy B that a subsequent recording must still protect.
#[derive(Clone)]
struct SwitchingOutput(Arc<Mutex<Arc<FakeOutput>>>);

impl OutputBackend for SwitchingOutput {
    fn begin(&self, _: u64) {}

    fn mute(&self) -> Result<Option<Box<dyn MutedOutput>>, String> {
        FakeBackend(self.0.lock().unwrap().clone()).mute()
    }

    fn pause(&self, _: u64) {}
    fn resume(&self) {}
    fn drain(&self) {}
}

fn mute_only() -> OutputOptions {
    OutputOptions {
        mute: true,
        pause: false,
    }
}

#[test]
fn failed_restore_does_not_disable_new_default_mute() {
    let original = Arc::new(FakeOutput::default());
    let replacement = Arc::new(FakeOutput::default());
    let output = SwitchingOutput(Arc::new(Mutex::new(original.clone())));
    let controller = RecordingOutputController::with_backend(Arc::new(output.clone()));
    controller.begin(1, mute_only());
    controller.ready(1);
    original.restore_error.store(true, Ordering::SeqCst);
    controller.finish_after_capture(1, || ());

    *output.0.lock().unwrap() = replacement.clone();
    controller.begin(2, mute_only());
    controller.ready(2);
    assert!(replacement.muted.load(Ordering::SeqCst));
    assert_eq!(controller.0.state.lock().unwrap().muted.len(), 2);
    controller.finish_after_capture(2, || {
        assert!(original.muted.load(Ordering::SeqCst));
        assert!(replacement.muted.load(Ordering::SeqCst));
    });
    assert!(!replacement.muted.load(Ordering::SeqCst));
    assert!(original.muted.load(Ordering::SeqCst));
    assert_eq!(controller.0.state.lock().unwrap().muted.len(), 1);

    original.restore_error.store(false, Ordering::SeqCst);
    controller.close_after_capture(|| ());
    assert!(!original.muted.load(Ordering::SeqCst));
    assert!(!replacement.muted.load(Ordering::SeqCst));
}

#[test]
fn returning_to_retained_output_never_restores_into_a_new_capture() {
    let original = Arc::new(FakeOutput::default());
    let replacement = Arc::new(FakeOutput::default());
    let output = SwitchingOutput(Arc::new(Mutex::new(original.clone())));
    let controller = RecordingOutputController::with_backend(Arc::new(output.clone()));
    controller.begin(1, mute_only());
    controller.ready(1);
    original.restore_error.store(true, Ordering::SeqCst);
    controller.finish_after_capture(1, || ());
    *output.0.lock().unwrap() = replacement.clone();
    controller.begin(2, mute_only());
    controller.ready(2);
    controller.finish_after_capture(2, || ());

    *output.0.lock().unwrap() = original.clone();
    original.restore_error.store(false, Ordering::SeqCst);
    let attempts = original.restore_calls.load(Ordering::SeqCst);
    controller.begin(3, mute_only());
    assert!(original.muted.load(Ordering::SeqCst));
    controller.ready(3);
    assert_eq!(original.restore_calls.load(Ordering::SeqCst), attempts);
    assert_eq!(controller.0.state.lock().unwrap().muted.len(), 1);
    controller.finish_after_capture(3, || {
        assert!(original.muted.load(Ordering::SeqCst));
        assert!(!replacement.muted.load(Ordering::SeqCst));
    });
    assert!(!original.muted.load(Ordering::SeqCst));
    assert_eq!(original.restore_calls.load(Ordering::SeqCst), attempts + 1);
}

#[test]
fn pending_restoration_limit_never_creates_an_untracked_mute() {
    let output = SwitchingOutput(Arc::new(Mutex::new(Arc::new(FakeOutput::default()))));
    let controller = RecordingOutputController::with_backend(Arc::new(output.clone()));
    for generation in 1..=MAX_MUTE_LEASES as u64 + 1 {
        let next = Arc::new(FakeOutput::default());
        next.restore_error.store(true, Ordering::SeqCst);
        *output.0.lock().unwrap() = next.clone();
        controller.begin(generation, mute_only());
        controller.ready(generation);
        assert_eq!(
            next.muted.load(Ordering::SeqCst),
            generation <= MAX_MUTE_LEASES as u64
        );
        controller.finish_after_capture(generation, || ());
        assert!(controller.0.state.lock().unwrap().muted.len() <= MAX_MUTE_LEASES);
    }
}

#[test]
fn settings_are_independent_snapshots_for_each_recording() {
    let (controller, output) = controller();
    controller.begin(1, both());
    let next = OutputOptions::default();
    controller.ready(1);
    controller.stop_requested(1);
    assert_eq!(output.protected(), (true, true));
    controller.finish_after_capture(1, || ());
    controller.begin(2, next);
    controller.ready(2);
    assert_eq!(output.protected(), (false, false));
    controller.finish_after_capture(2, || ());
}

#[test]
fn capture_error_still_restores_after_capture_is_quiescent() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    let result = controller.finish_after_capture(1, || {
        output.event("capture_stopped");
        Err::<(), _>("sample retrieval failed")
    });
    assert_eq!(result, Some(Err("sample retrieval failed")));
    assert_eq!(output.protected(), (false, false));
}

#[test]
fn stream_close_and_exit_restore_only_after_capture_is_closed() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    controller.close_after_capture(|| {
        assert_eq!(output.protected(), (true, true));
        output.event("capture_closed");
    });
    assert_eq!(output.protected(), (false, false));
    controller.restore_before_exit();
    assert_eq!(output.restore_calls.load(Ordering::SeqCst), 1);
    controller.close_after_capture(|| ());
}

#[test]
fn overlapping_begin_cannot_release_an_active_recording() {
    let (controller, output) = controller();
    controller.begin(1, both());
    controller.ready(1);
    controller.begin(2, OutputOptions::default());
    controller.ready(2);
    assert_eq!(output.protected(), (true, true));
    controller.finish_after_capture(1, || ());
}

#[test]
fn system_controller_construction_does_not_touch_media_or_mute() {
    // No ready call: neither backend enumerates or changes the desktop.
    let controller = RecordingOutputController::new();
    controller.restore_before_exit();
}
