//! Per-recording reminders for toggle/locked dictation.

#[path = "recording_reminders_core.rs"]
mod core;

pub(crate) use core::ReminderLease;
use core::ReminderSchedule;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

#[derive(Default)]
pub struct RecordingReminders(Mutex<Option<Session>>);

struct Session {
    started: Instant,
    lease: ReminderLease,
    locked: bool,
    enabled: bool,
    first_after: Duration,
    sound: bool,
}

/// Called only after Start was accepted. A hold session records its clock but
/// creates no timer unless the coordinator later classifies it as a tap.
pub fn start(app: &AppHandle, locked: bool) {
    let Some(reminders) = app.try_state::<RecordingReminders>() else {
        return;
    };
    let settings = crate::settings::get_settings(app);
    let Ok(mut current) = reminders.0.lock() else {
        return;
    };
    if let Some(previous) = current.take() {
        previous.lease.cancel();
    }
    let session = Session {
        started: Instant::now(),
        lease: ReminderLease::new(),
        locked,
        enabled: settings.recording_reminders_enabled,
        first_after: Duration::from_secs(
            u64::from(settings.recording_reminder_minutes.clamp(1, 120)) * 60,
        ),
        sound: settings.recording_reminder_sound,
    };
    if session.enabled && session.locked {
        spawn_timer(app.clone(), &session);
    }
    *current = Some(session);
}

/// Hold-or-toggle became a locked recording after the shortcut was released.
/// Repeated calls cannot spawn duplicate reminder workers.
pub fn lock(app: &AppHandle) {
    let Some(reminders) = app.try_state::<RecordingReminders>() else {
        return;
    };
    let Ok(mut current) = reminders.0.lock() else {
        return;
    };
    if let Some(session) = current.as_mut() {
        if !session.locked {
            session.locked = true;
            if session.enabled {
                spawn_timer(app.clone(), session);
            }
        }
    }
}

/// Synchronous invalidation belongs before any asynchronous Stop/Cancel work.
/// The lease also rejects already queued main-thread shows and audio starts.
pub fn stop(app: &AppHandle) {
    let Some(reminders) = app.try_state::<RecordingReminders>() else {
        return;
    };
    if let Ok(mut current) = reminders.0.lock() {
        if let Some(session) = current.take() {
            session.lease.cancel();
        }
    }
    crate::notifications::dismiss_recording_reminder(app);
}

fn spawn_timer(app: AppHandle, session: &Session) {
    let started = session.started;
    let lease = session.lease.clone();
    let sound = session.sound;
    let mut schedule = ReminderSchedule::new(session.first_after);
    std::thread::spawn(move || loop {
        // The bounded waits also avoid passing platform-overflowing durations
        // to a condition variable after many doublings. They do not emit extra
        // reminders or depend on speech activity.
        let wait = schedule
            .wait_for(started.elapsed())
            .min(Duration::from_secs(24 * 60 * 60));
        if !lease.wait(wait) {
            break;
        }
        let elapsed = started.elapsed();
        if schedule.poll(elapsed) {
            crate::notifications::show_recording_reminder(
                &app,
                elapsed.as_secs(),
                lease.clone(),
                sound,
            );
        }
    });
}

impl Drop for RecordingReminders {
    fn drop(&mut self) {
        if let Ok(current) = self.0.get_mut() {
            if let Some(session) = current.take() {
                session.lease.cancel();
            }
        }
    }
}
