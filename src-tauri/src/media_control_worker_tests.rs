use super::*;
use std::sync::mpsc;

#[derive(Default)]
struct Calls {
    pauses: usize,
    resumed: Vec<usize>,
}

struct Backend {
    calls: Arc<Mutex<Calls>>,
    block: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    pause_succeeds: bool,
}

impl PlaybackBackend for Backend {
    type Paused = usize;

    fn pause_playback(&mut self) -> Result<Option<usize>, String> {
        let id = {
            let mut calls = self.calls.lock().unwrap();
            calls.pauses += 1;
            calls.pauses
        };
        if let Some((entered, release)) = self.block.take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Ok(self.pause_succeeds.then_some(id))
    }

    fn resume_playback(&mut self, paused: usize) -> Result<(), String> {
        self.calls.lock().unwrap().resumed.push(paused);
        Ok(())
    }
}

fn worker() -> (PlaybackWorker, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let worker = PlaybackWorker::new(Backend {
        calls: calls.clone(),
        block: None,
        pause_succeeds: true,
    });
    (worker, calls)
}

fn settled(worker: &PlaybackWorker) {
    assert!(worker.wait_until_settled(Duration::from_secs(5)));
}

#[test]
fn begin_without_apply_does_not_pause_media() {
    let (worker, calls) = worker();
    worker.begin(1, true);
    settled(&worker);
    worker.remove();
    settled(&worker);
    assert_eq!(calls.lock().unwrap().pauses, 0);
}

#[test]
fn stale_apply_cannot_pause_a_new_recording() {
    let (worker, calls) = worker();
    worker.begin(1, true);
    worker.remove();
    worker.begin(2, true);
    worker.apply(1);
    settled(&worker);
    assert_eq!(calls.lock().unwrap().pauses, 0);
    worker.apply(2);
    settled(&worker);
    assert_eq!(calls.lock().unwrap().pauses, 1);
}

#[test]
fn duplicate_apply_and_finish_only_change_playback_once() {
    let (worker, calls) = worker();
    worker.begin(1, true);
    worker.apply(1);
    settled(&worker);
    worker.apply(1);
    settled(&worker);
    worker.remove();
    settled(&worker);
    worker.remove();
    settled(&worker);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.pauses, 1);
    assert_eq!(calls.resumed, [1]);
}

#[test]
fn play_after_recording_disabled_leaves_the_owned_playback_paused() {
    let (worker, calls) = worker();
    worker.begin(1, false);
    worker.apply(1);
    settled(&worker);
    worker.remove();
    settled(&worker);
    assert!(calls.lock().unwrap().resumed.is_empty());
}

fn late_pause(resume: bool, next: bool) -> Arc<Mutex<Calls>> {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (entered, entering) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let worker = PlaybackWorker::new(Backend {
        calls: calls.clone(),
        block: Some((entered, blocked)),
        pause_succeeds: true,
    });
    worker.begin(1, resume);
    worker.apply(1);
    entering.recv_timeout(Duration::from_secs(5)).unwrap();

    // These are the operations called under RecordingOutputController's mutex.
    // They must complete while the provider remains blocked on the other thread.
    let changing = worker.clone();
    let (changed, complete) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        changing.remove();
        if next {
            changing.begin(2, !resume);
            changing.apply(2);
        }
        changed.send(()).unwrap();
    });
    let nonblocking = complete.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    thread.join().unwrap();
    assert!(
        nonblocking,
        "provider calls must not hold the request mutex"
    );
    settled(&worker);
    worker.remove();
    settled(&worker);
    calls
}

#[test]
fn late_success_is_restored_after_finish_without_blocking_capture_cleanup() {
    let calls = late_pause(true, false);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.pauses, 1);
    assert_eq!(calls.resumed, [1]);
}

#[test]
fn late_success_keeps_the_original_no_resume_policy() {
    let calls = late_pause(false, false);
    assert!(calls.lock().unwrap().resumed.is_empty());
}

#[test]
fn next_recording_cannot_change_the_previous_resume_policy() {
    let calls = late_pause(true, true);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.pauses, 2);
    assert_eq!(calls.resumed, [1]);

    drop(calls);
    let calls = late_pause(false, true);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.pauses, 2);
    assert_eq!(calls.resumed, [2]);
}

#[test]
fn unsuccessful_pause_never_issues_a_resume() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let worker = PlaybackWorker::new(Backend {
        calls: calls.clone(),
        block: None,
        pause_succeeds: false,
    });
    worker.begin(1, true);
    worker.apply(1);
    settled(&worker);
    worker.restore_before_exit();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.pauses, 1);
    assert!(calls.resumed.is_empty());
}

#[test]
fn cancelled_next_recording_is_not_paused_after_an_old_call_finishes() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (entered, entering) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let worker = PlaybackWorker::new(Backend {
        calls: calls.clone(),
        block: Some((entered, blocked)),
        pause_succeeds: true,
    });
    worker.begin(1, true);
    worker.apply(1);
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.remove();
    worker.begin(2, true);
    worker.apply(2);
    worker.remove();
    release.send(()).unwrap();
    settled(&worker);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.pauses, 1);
    assert_eq!(calls.resumed, [1]);
}
