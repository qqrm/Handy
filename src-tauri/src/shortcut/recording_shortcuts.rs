//! Recording-only hotkeys share one serialized reconciliation path.

use super::recording_scope::RecordingScope;
use crate::settings::{self, KeyboardImplementation, ShortcutBinding};
use log::warn;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

pub(super) fn is_recording_binding(id: &str) -> bool {
    matches!(id, "cancel" | "transcribe_to_clipboard")
}

struct Registration {
    implementation: KeyboardImplementation,
    binding: ShortcutBinding,
    scope: u64,
}

#[derive(Default)]
struct RecordingShortcuts {
    scope: RecordingScope,
    registrations: Mutex<Vec<Registration>>,
}

fn initialize(app: &AppHandle) {
    if app.try_state::<RecordingShortcuts>().is_none() {
        app.manage(RecordingShortcuts::default());
    }
}

pub(super) fn current_scope(app: &AppHandle) -> Option<u64> {
    app.try_state::<RecordingShortcuts>()
        .and_then(|state| state.scope.snapshot().token())
}

pub(super) fn accepts(app: &AppHandle, token: u64) -> bool {
    app.try_state::<RecordingShortcuts>()
        .is_some_and(|state| state.scope.accepts(token))
}

pub(super) fn start(app: &AppHandle) {
    initialize(app);
    app.state::<RecordingShortcuts>().scope.start();
    schedule(app);
}

pub(super) fn stop(app: &AppHandle) {
    initialize(app);
    app.state::<RecordingShortcuts>().scope.stop();
    schedule(app);
}

pub(super) fn refresh(app: &AppHandle) {
    initialize(app);
    app.state::<RecordingShortcuts>().scope.refresh();
    schedule(app);
}

pub(super) fn suspend(app: &AppHandle) {
    initialize(app);
    app.state::<RecordingShortcuts>().scope.suspend();
    schedule(app);
}

pub(super) fn resume(app: &AppHandle) {
    initialize(app);
    app.state::<RecordingShortcuts>().scope.resume();
    schedule(app);
}

fn schedule(app: &AppHandle) {
    let app = app.clone();
    // A handy-keys callback runs on its manager thread. Never synchronously
    // wait for that same thread to answer a register/unregister request.
    tauri::async_runtime::spawn_blocking(move || reconcile(&app));
}

fn reconcile(app: &AppHandle) {
    let state = app.state::<RecordingShortcuts>();
    let Ok(mut registered) = state.registrations.lock() else {
        warn!("Recording shortcut registration lock is poisoned");
        return;
    };
    loop {
        let snapshot = state.scope.snapshot();
        let settings = settings::get_settings(app);
        // Existing dynamic shortcut registration is disabled on Linux. Do not
        // turn Ctrl+Escape into a permanent global binding as a workaround.
        let scope = snapshot.token().filter(|_| !cfg!(target_os = "linux"));
        let wanted: Vec<_> = settings
            .bindings
            .values()
            .filter(|binding| is_recording_binding(&binding.id))
            .collect();

        let mut kept = Vec::new();
        for registration in registered.drain(..) {
            let unchanged = scope == Some(registration.scope)
                && registration.implementation == settings.keyboard_implementation
                && wanted.iter().any(|binding| {
                    binding.id == registration.binding.id
                        && binding.current_binding == registration.binding.current_binding
                });
            if unchanged {
                kept.push(registration);
                continue;
            }
            let result = match registration.implementation {
                KeyboardImplementation::Tauri => {
                    super::tauri_impl::unregister_shortcut(app, registration.binding.clone())
                }
                KeyboardImplementation::HandyKeys => {
                    super::handy_keys::unregister_shortcut(app, registration.binding.clone())
                }
            };
            if let Err(error) = result {
                warn!(
                    "Could not unregister recording shortcut '{}': {error}",
                    registration.binding.id
                );
                // Retain exact ownership for the next cleanup attempt.
                kept.push(registration);
            }
        }
        *registered = kept;

        if let Some(scope) = scope {
            for binding in wanted {
                if !state.scope.accepts(scope) {
                    break;
                }
                if registered.iter().any(|entry| {
                    entry.implementation == settings.keyboard_implementation
                        && entry.binding.id == binding.id
                }) {
                    continue;
                }
                let result = match settings.keyboard_implementation {
                    KeyboardImplementation::Tauri => super::tauri_impl::register_scoped_shortcut(
                        app,
                        binding.clone(),
                        Some(scope),
                    ),
                    KeyboardImplementation::HandyKeys => {
                        super::handy_keys::register_scoped_shortcut(
                            app,
                            binding.clone(),
                            Some(scope),
                        )
                    }
                };
                match result {
                    Ok(()) => registered.push(Registration {
                        implementation: settings.keyboard_implementation,
                        binding: binding.clone(),
                        scope,
                    }),
                    Err(error) => warn!(
                        "Could not register recording shortcut '{}': {error}",
                        binding.id
                    ),
                }
            }
        }
        // A Stop, edit or new Start that overtook plugin work wins before this
        // worker leaves; a stale registration is removed in the same worker.
        if snapshot == state.scope.snapshot() {
            break;
        }
    }
}
