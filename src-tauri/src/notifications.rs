//! Small, non-activating notifications shared by reminders and text delivery.

use crate::recording_reminders::ReminderLease;
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Listener, Manager};

const WINDOW: &str = "handy_notification";
const EVENT: &str = "handy-notification";
const READY: &str = "handy-notification-ready";
const WIDTH: f64 = 420.0;
const HEIGHT: f64 = 170.0;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryFailureReason {
    Unavailable,
    Changed,
    Unsupported,
    Uncertain,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Notification {
    Recording { elapsed_seconds: u64 },
    DeliveryFallback { reason: DeliveryFailureReason },
    ClipboardError,
    AutoSubmitSkipped,
}

#[derive(Serialize)]
struct NotificationPayload {
    #[serde(flatten)]
    message: Notification,
    language: String,
    theme: crate::settings::Theme,
}

struct Pending {
    id: u64,
    message: Notification,
    lease: Option<ReminderLease>,
    sound: bool,
}

struct Displayed {
    id: u64,
    lease: Option<ReminderLease>,
}

#[derive(Default)]
struct NotificationState {
    ready: bool,
    next_id: u64,
    pending: Option<Pending>,
    displayed: Option<Displayed>,
}

#[derive(Default)]
struct Notifications(Mutex<NotificationState>);

/// Initialize during setup so the popup is ready before the first timer. The
/// readiness handshake retains a pending message if WebView loading is slow.
pub fn init(app: &AppHandle) {
    if app.try_state::<Notifications>().is_none() {
        app.manage(Notifications::default());
        let handle = app.clone();
        app.listen(READY, move |_| {
            let handle = handle.clone();
            let queue = handle.clone();
            let _ = queue.run_on_main_thread(move || {
                let state = handle.state::<Notifications>();
                let pending = state.0.lock().ok().and_then(|mut state| {
                    state.ready = true;
                    state.pending.take()
                });
                if let Some(pending) = pending {
                    display_on_main(&handle, pending);
                }
            });
        });
    }
    if app.get_webview_window(WINDOW).is_some() {
        return;
    }
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        WINDOW,
        tauri::WebviewUrl::App("src/notification/index.html".into()),
    )
    .title("Handy")
    .inner_size(WIDTH, HEIGHT)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focusable(false)
    .focused(false)
    .visible(false);
    if let Some(data_dir) = crate::portable::data_dir() {
        builder = builder.data_directory(data_dir.join("webview"));
    }
    match builder.build() {
        Ok(window) => {
            if let Err(error) = window.set_ignore_cursor_events(true) {
                log::warn!("Failed to make notification click-through: {error}");
            }
        }
        Err(error) => log::warn!("Failed to create notification window: {error}"),
    }
}

pub(crate) fn show_recording_reminder(
    app: &AppHandle,
    elapsed_seconds: u64,
    lease: ReminderLease,
    sound: bool,
) {
    enqueue(
        app,
        Notification::Recording { elapsed_seconds },
        Some(lease),
        sound,
    );
}

/// Call only after the clipboard write succeeded. The delivery caller owns the
/// user's optional-notification setting and maps errors to these coarse reasons.
pub fn show_delivery_fallback(app: &AppHandle, reason: DeliveryFailureReason) {
    enqueue(app, Notification::DeliveryFallback { reason }, None, false);
}

pub fn show_clipboard_error(app: &AppHandle) {
    enqueue(app, Notification::ClipboardError, None, false);
}

pub fn show_auto_submit_skipped(app: &AppHandle) {
    enqueue(app, Notification::AutoSubmitSkipped, None, false);
}

fn enqueue(app: &AppHandle, message: Notification, lease: Option<ReminderLease>, sound: bool) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        init(&handle);
        let state = handle.state::<Notifications>();
        let gate = lease.clone();
        let enqueue = || {
            state.0.lock().ok().and_then(|mut state| {
                state.next_id = state.next_id.wrapping_add(1);
                let pending = Pending {
                    id: state.next_id,
                    message,
                    lease,
                    sound,
                };
                if state.ready {
                    Some(pending)
                } else {
                    state.pending = Some(pending);
                    None
                }
            })
        };
        // A cancelled reminder must not replace a genuine delivery notice
        // while the WebView is still loading. Gate the queue mutation itself.
        let pending = match gate {
            Some(lease) => lease.with_active(enqueue).flatten(),
            None => enqueue(),
        };
        if let Some(pending) = pending {
            display_on_main(&handle, pending);
        }
    });
}

fn display_on_main(app: &AppHandle, pending: Pending) {
    let display = || {
        let Some(window) = app.get_webview_window(WINDOW) else {
            return false;
        };
        let settings = crate::settings::get_settings(app);
        let payload = NotificationPayload {
            message: pending.message.clone(),
            language: settings.app_language,
            theme: settings.theme,
        };
        if let Err(error) = window.emit(EVENT, &payload) {
            log::warn!("Failed to render notification: {error}");
            return false;
        }
        if let Err(error) = show_without_focus(app, &window) {
            log::warn!("Failed to show notification: {error}");
            return false;
        }
        if let Ok(mut state) = app.state::<Notifications>().0.lock() {
            state.displayed = Some(Displayed {
                id: pending.id,
                lease: pending.lease.clone(),
            });
        }
        true
    };
    let shown = match &pending.lease {
        Some(lease) => lease.with_active(display).unwrap_or(false),
        None => display(),
    };
    if !shown {
        return;
    }
    if pending.sound {
        if let Some(lease) = pending.lease {
            crate::audio_feedback::play_reminder_sound(app, lease);
        }
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
        let queue = handle.clone();
        let _ = queue.run_on_main_thread(move || {
            let state = handle.state::<Notifications>();
            if let Ok(mut state) = state.0.lock() {
                if state
                    .displayed
                    .as_ref()
                    .is_some_and(|shown| shown.id == pending.id)
                {
                    state.displayed = None;
                    if let Some(window) = handle.get_webview_window(WINDOW) {
                        let _ = window.hide();
                    }
                }
            };
        });
    });
}

/// An old Stop callback may run after another session begins. Hide only a
/// cancelled lease, never a new reminder or a delivery/clipboard notification.
pub(crate) fn dismiss_recording_reminder(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(state) = handle.try_state::<Notifications>() else {
            return;
        };
        if let Ok(mut state) = state.0.lock() {
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| cancelled(&pending.lease))
            {
                state.pending = None;
            }
            if state
                .displayed
                .as_ref()
                .is_some_and(|shown| cancelled(&shown.lease))
            {
                state.displayed = None;
                if let Some(window) = handle.get_webview_window(WINDOW) {
                    let _ = window.hide();
                }
            }
        };
    });
}

fn cancelled(lease: &Option<ReminderLease>) -> bool {
    lease
        .as_ref()
        .is_some_and(|lease| lease.with_active(|| ()).is_none())
}

fn show_without_focus(app: &AppHandle, window: &tauri::WebviewWindow) -> tauri::Result<()> {
    let monitor = notification_monitor(app);
    let scale = monitor
        .as_ref()
        .map_or(1.0, |monitor| monitor.scale_factor());
    #[cfg(target_os = "windows")]
    let text_scale = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Accessibility")
        .and_then(|key| key.get_value::<u32, _>("TextScaleFactor"))
        .map(|percent| (f64::from(percent) / 100.0).clamp(1.0, 2.25))
        .unwrap_or(1.0);
    #[cfg(not(target_os = "windows"))]
    let text_scale = 1.0;
    let width = (WIDTH * scale * text_scale).round() as i32;
    let height = (HEIGHT * scale * text_scale).round() as i32;
    let inset = (24.0 * scale).round() as i32;
    let (x, y) = monitor.as_ref().map_or((inset, inset), |monitor| {
        (
            monitor.position().x + monitor.size().width as i32 - width - inset,
            monitor.position().y + inset,
        )
    });

    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        };
        // The native call makes non-activation explicit, including DPI/size
        // changes. No set_focus, activation or synthetic keyboard input here.
        SetWindowPos(
            window.hwnd()?,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        .map_err(|error| tauri::Error::Anyhow(anyhow::anyhow!(error)))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        window.set_size(tauri::PhysicalSize::new(width as u32, height as u32))?;
        window.set_position(tauri::PhysicalPosition::new(x, y))?;
        window.show()?;
    }
    Ok(())
}

fn notification_monitor(app: &AppHandle) -> Option<tauri::Monitor> {
    if let Some((x, y)) = crate::input::get_cursor_position(app) {
        if let Ok(monitors) = app.available_monitors() {
            for monitor in monitors {
                #[cfg(target_os = "windows")]
                let scale = 1.0;
                #[cfg(not(target_os = "windows"))]
                let scale = monitor.scale_factor();
                let left = f64::from(monitor.position().x) / scale;
                let top = f64::from(monitor.position().y) / scale;
                let right = left + f64::from(monitor.size().width) / scale;
                let bottom = top + f64::from(monitor.size().height) / scale;
                if f64::from(x) >= left
                    && f64::from(x) < right
                    && f64::from(y) >= top
                    && f64::from(y) < bottom
                {
                    return Some(monitor);
                }
            }
        }
    }
    app.primary_monitor().ok().flatten()
}
