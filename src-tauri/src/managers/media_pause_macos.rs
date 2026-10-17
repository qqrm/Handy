//! QuickTime-only public Apple Events. Observed overrides revoke ownership;
//! polling cannot detect every Play/Pause pair or same-file in-place replacement.
use super::{
    MediaBackend, MediaOperation, MediaResult, MediaSession, PauseOwnership, PlaybackState,
    SessionWatch, MAX_MEDIA_LEASES,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, Mutex, Weak};
use std::time::Duration;

extern "C" {
    fn handy_qt_discover(count: *mut usize, status: *mut i32) -> *mut c_void;
    fn handy_qt_session(list: *mut c_void, index: usize) -> *mut c_void;
    fn handy_qt_release(value: *mut c_void);
    fn handy_qt_live(value: *mut c_void) -> i32;
    fn handy_qt_state(value: *mut c_void, position: *mut f64) -> i32;
    fn handy_qt_command(value: *mut c_void, play: i32,
        allowed: extern "C" fn(*mut c_void, f64) -> bool, context: *mut c_void) -> i32;
    fn handy_qt_request_permission() -> i32;
}

/// Only call from an explicit settings action, off the UI thread. Never launches
/// QuickTime, and the recording path never asks for Automation authorization.
pub(super) fn request_quicktime_permission() -> MediaResult<bool> {
    match unsafe { handy_qt_request_permission() } {
        1 => Ok(true),
        0 => Ok(false),
        _ => Err("QuickTime Automation permission was denied or unavailable".into()),
    }
}

struct NativeTarget(NonNull<c_void>);
// The shim publishes immutable Foundation objects; every invocation owns its
// autorelease pool and event/reply descriptors. No Objective-C object is mutated.
unsafe impl Send for NativeTarget {}
unsafe impl Sync for NativeTarget {}
impl Drop for NativeTarget {
    fn drop(&mut self) { unsafe { handy_qt_release(self.0.as_ptr()) }; }
}

pub(super) struct Backend;
impl MediaBackend for Backend {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        let mut count = 0;
        let mut status = -1;
        let raw = unsafe { handy_qt_discover(&mut count, &mut status) };
        let Some(list) = NonNull::new(raw).map(NativeTarget) else {
            return Err("QuickTime discovery or Automation authorization unavailable".into());
        };
        if status != 0 || count > MAX_MEDIA_LEASES {
            return Err("QuickTime discovery exceeded safe bounds".into());
        }
        (0..count).map(|index| {
            let target = NonNull::new(unsafe { handy_qt_session(list.0.as_ptr(), index) })
                .ok_or_else(|| "QuickTime discovery omitted a target".to_string())?;
            Ok(Arc::new(Session {
                target: Arc::new(NativeTarget(target)),
                observation: Arc::new(Mutex::new(PositionGuard::default())),
                snapshot_lock: Arc::new(Mutex::new(())),
                ownership: Arc::new(Mutex::new(Weak::new())),
            }) as Arc<dyn MediaSession>)
        }).collect()
    }
}

#[derive(Default)]
struct PositionGuard {
    paused_at: Option<f64>,
    lost: bool,
}
impl PositionGuard {
    fn observe(&mut self, state: i32, position: f64) -> MediaResult<PlaybackState> {
        if self.lost || !position.is_finite() || position < 0.0 {
            self.lost = true;
            return Err("QuickTime playback ownership was lost".into());
        }
        match state {
            1 if self.paused_at.is_none() => Ok(PlaybackState::Playing),
            0 => {
                if self.paused_at.is_some_and(|saved| (position - saved).abs() > 0.15) {
                    self.lost = true;
                    return Err("QuickTime position changed while paused".into());
                }
                self.paused_at.get_or_insert(position);
                Ok(PlaybackState::Paused)
            }
            1 => {
                self.lost = true;
                Ok(PlaybackState::Playing)
            }
            _ => {
                self.lost = true;
                Err("QuickTime target or state could not be verified".into())
            }
        }
    }
}

#[derive(Clone)]
struct Session {
    target: Arc<NativeTarget>,
    observation: Arc<Mutex<PositionGuard>>,
    snapshot_lock: Arc<Mutex<()>>,
    ownership: Arc<Mutex<Weak<PauseOwnership>>>,
}
impl Session {
    fn command(&self, play: bool) -> MediaResult<MediaOperation> {
        let session = self.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new().name("handy-quicktime-command".into()).spawn(move || {
            let mut context = CommandContext { session, play };
            let result = unsafe { handy_qt_command(context.session.target.0.as_ptr(), i32::from(play),
                command_allowed, (&mut context as *mut CommandContext).cast()) };
            if result == 2 {
                if let Some(owner) = context.session.ownership.lock().unwrap_or_else(|e| e.into_inner()).upgrade() {
                    owner.observe(Err("QuickTime command outcome is uncertain".into()));
                }
            }
            let _ = sender.send(result);
        }).map_err(|error| error.to_string())?;
        // Retain the exact target even AFTER the sending thread reports unknown.
        // No retry or timeout-to-rejection conversion; only original-process
        // disappearance permits the controller to release an uncertain effect.
        let retained = self.target.clone();
        let mut unresolved = false;
        Ok(Box::new(move || {
            let _keep_target = &retained;
            if unresolved { return None; }
            match receiver.try_recv() {
                Ok(1) => Some(Ok(true)),
                Ok(0) => Some(Ok(false)),
                Ok(-1) => Some(Err("QuickTime command preflight failed before submission".into())),
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => { unresolved = true; None }
                Err(mpsc::TryRecvError::Empty) => None,
            }
        }))
    }
}

struct CommandContext { session: Session, play: bool }
extern "C" fn command_allowed(raw: *mut c_void, position: f64) -> bool {
    // Synchronous callback only: the sending thread retains this stack context.
    let context = unsafe { &*(raw.cast::<CommandContext>()) };
    let observation = context.session.observation.lock().unwrap_or_else(|e| e.into_inner());
    if observation.lost || !position.is_finite() { return false; }
    if context.play && !observation.paused_at.is_some_and(|saved| (position - saved).abs() <= 0.15) {
        return false;
    }
    let ownership = context.session.ownership.lock().unwrap_or_else(|e| e.into_inner()).upgrade();
    ownership.is_some_and(|owner| if context.play { owner.can_resume() } else { !owner.lost() })
}

impl MediaSession for Session {
    fn state(&self) -> MediaResult<PlaybackState> {
        let _read = self.snapshot_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut position = f64::NAN;
        let state = unsafe { handy_qt_state(self.target.0.as_ptr(), &mut position) };
        self.observation.lock().unwrap_or_else(|e| e.into_inner()).observe(state, position)
    }
    fn pause(&self) -> MediaResult<MediaOperation> { self.command(false) }
    fn play(&self) -> MediaResult<MediaOperation> { self.command(true) }
    fn is_live(&self) -> MediaResult<bool> {
        match unsafe { handy_qt_live(self.target.0.as_ptr()) } {
            1 => Ok(true),
            0 => Ok(false),
            _ => Err("QuickTime original process liveness is unknown".into()),
        }
    }
    fn watch(&self, ownership: Arc<PauseOwnership>) -> MediaResult<Box<dyn SessionWatch>> {
        *self.ownership.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&ownership);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let session = self.clone();
        std::thread::Builder::new().name("handy-quicktime-observer".into()).spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let _ = ownership.snapshot(&session);
                std::thread::sleep(Duration::from_millis(250));
            }
        }).map_err(|error| error.to_string())?;
        Ok(Box::new(Watch(stop)))
    }
}
struct Watch(Arc<AtomicBool>);
impl SessionWatch for Watch {}
impl Drop for Watch {
    fn drop(&mut self) { self.0.store(true, Ordering::Release); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observed_resume_and_seek_permanently_revoke_ownership() {
        let mut state = PositionGuard::default();
        assert_eq!(state.observe(1, 4.0), Ok(PlaybackState::Playing));
        assert_eq!(state.observe(0, 4.1), Ok(PlaybackState::Paused));
        assert_eq!(state.observe(1, 4.2), Ok(PlaybackState::Playing));
        assert!(state.observe(0, 4.2).is_err());
        let mut state = PositionGuard::default();
        assert_eq!(state.observe(0, 4.0), Ok(PlaybackState::Paused));
        assert!(state.observe(0, 5.0).is_err());
        assert!(state.observe(0, 4.0).is_err());
    }
    #[test]
    fn invalid_or_unknown_snapshot_is_fail_closed() {
        for (code, position) in [(-1, 0.0), (0, f64::NAN), (0, -1.0)] {
            let mut state = PositionGuard::default();
            assert!(state.observe(code, position).is_err());
            assert!(state.observe(0, 1.0).is_err());
        }
    }
}
