//! Compile the unchanged output/session module without building the microphone,
//! ASR engine, WebView or desktop UI. All external fakes are declared here.
//! Windows builds use the real dictation_target and Windows key API.

extern crate self as tauri;

use std::any::Any;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

#[path = "../../src/dictation_target/mod.rs"]
pub mod dictation_target;

#[path = "../../src/dictation_output/mod.rs"]
pub mod dictation_output;

/// Minimal stand-in for Tauri's state borrowing, with the same lifetime shape.
pub struct State<'a, T: Send + Sync + 'static> {
    value: &'a T,
}

impl<T: Send + Sync + 'static> Deref for State<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.value
    }
}

pub trait Manager {
    fn try_state<T: Send + Sync + 'static>(&self) -> Option<State<'_, T>>;
}

#[derive(Clone, Default)]
pub struct AppHandle(Arc<FixtureState>);

#[derive(Default)]
struct FixtureState {
    output: dictation_output::DictationOutputState,
    settings: Mutex<settings::AppSettings>,
    effects: Mutex<Effects>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub clipboard_writes: Vec<String>,
    pub guarded_prepares: Vec<(String, bool)>,
    pub guarded_restores: usize,
    pub clipboard_was_replaced: bool,
    pub paste_calls: Vec<String>,
    pub notices: Vec<String>,
    pub fail_clipboard: bool,
}

impl AppHandle {
    pub fn effects(&self) -> Effects {
        self.0.effects.lock().expect("fixture effects").clone()
    }
    pub fn configure(&self, update: impl FnOnce(&mut settings::AppSettings)) {
        update(&mut self.0.settings.lock().expect("fixture settings"));
    }
    pub fn fail_clipboard(&self) {
        self.0
            .effects
            .lock()
            .expect("fixture effects")
            .fail_clipboard = true;
    }
}

impl Manager for AppHandle {
    fn try_state<T: Send + Sync + 'static>(&self) -> Option<State<'_, T>> {
        (&self.0.output as &dyn Any)
            .downcast_ref()
            .map(|value| State { value })
    }
}

/// External settings transport only; no output decisions live in this fake.
pub mod settings {
    use super::AppHandle;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum AutoSubmitKey {
        #[default]
        Enter,
        CtrlEnter,
        CmdEnter,
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum ClipboardHandling {
        #[default]
        DontModify,
        CopyToClipboard,
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum DictationTarget {
        #[default]
        RecordingStart,
        RecordingEnd,
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum PasteMethod {
        #[default]
        CtrlV,
        Direct,
        None,
        ShiftInsert,
        CtrlShiftV,
        ExternalScript,
    }

    #[derive(Clone, Debug, Default)]
    pub struct AppSettings {
        pub append_trailing_space: bool,
        pub paste_method: PasteMethod,
        pub clipboard_handling: ClipboardHandling,
        pub auto_submit: bool,
        pub auto_submit_key: AutoSubmitKey,
        pub notify_delivery_failure: bool,
        pub dictation_target: DictationTarget,
        pub paste_delay_ms: u64,
        pub paste_delay_after_ms: u64,
    }

    pub fn get_settings(app: &AppHandle) -> AppSettings {
        app.0.settings.lock().expect("fixture settings").clone()
    }
}

/// Fake clipboard transport: records exact bytes and can fail deterministically.
pub mod clipboard {
    use super::AppHandle;
    pub fn write_text_to_clipboard(app: &AppHandle, text: &str) -> Result<(), String> {
        let mut effects = app.0.effects.lock().expect("fixture effects");
        if effects.fail_clipboard {
            return Err("clipboard unavailable in fixture".into());
        }
        effects.clipboard_writes.push(text.to_owned());
        Ok(())
    }

    pub struct GuardedClipboard {
        restore: bool,
        app: AppHandle,
    }

    pub fn prepare_guarded_paste_clipboard(
        app: &AppHandle,
        text: &str,
        restore: bool,
    ) -> Result<GuardedClipboard, String> {
        write_text_to_clipboard(app, text)?;
        app.0
            .effects
            .lock()
            .expect("fixture effects")
            .guarded_prepares
            .push((text.into(), restore));
        Ok(GuardedClipboard {
            restore,
            app: app.clone(),
        })
    }

    impl GuardedClipboard {
        pub fn is_current(&self) -> bool {
            !self.app.effects().clipboard_was_replaced
        }

        pub fn restore_after_verified_paste(self, app: &AppHandle) -> Result<(), String> {
            if self.restore && self.is_current() {
                app.0
                    .effects
                    .lock()
                    .expect("fixture effects")
                    .guarded_restores += 1;
            }
            Ok(())
        }
    }
}

/// Non-Windows legacy paste transport only, never a substitute target matcher.
pub mod utils {
    use super::AppHandle;
    pub fn paste(text: String, app: AppHandle) -> Result<(), String> {
        app.0
            .effects
            .lock()
            .expect("fixture effects")
            .paste_calls
            .push(text);
        Ok(())
    }
}

pub mod notifications {
    use super::AppHandle;
    #[derive(Debug)]
    pub enum DeliveryFailureReason {
        Unavailable,
        Changed,
        Unsupported,
        Uncertain,
    }
    pub fn show_clipboard_error(app: &AppHandle) {
        record(app, "clipboard_error".into());
    }
    pub fn show_auto_submit_skipped(app: &AppHandle) {
        record(app, "auto_submit_skipped".into());
    }
    pub fn show_delivery_fallback(app: &AppHandle, reason: DeliveryFailureReason) {
        record(app, format!("fallback:{reason:?}"));
    }
    fn record(app: &AppHandle, notice: String) {
        app.0
            .effects
            .lock()
            .expect("fixture effects")
            .notices
            .push(notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dictation_output::{begin, cancel, deliver, prepare_stop, take, OutputMode};

    #[test]
    fn prepared_output_can_move_to_the_blocking_delivery_worker() {
        fn assert_send<T: Send>() {}
        assert_send::<dictation_output::PreparedOutput>();
    }

    fn clipboard_session(app: &AppHandle) -> Option<dictation_output::PreparedOutput> {
        // No capture is performed at Start when End is configured; the
        // alternative stop must not perform the deferred capture either.
        app.configure(|settings| {
            settings.dictation_target = settings::DictationTarget::RecordingEnd
        });
        begin(app);
        prepare_stop(app, OutputMode::ClipboardOnly);
        take(app)
    }

    #[test]
    fn clipboard_stop_preserves_the_entire_result_and_bypasses_paste() {
        let app = AppHandle::default();
        app.configure(|settings| {
            settings.auto_submit = true;
            settings.append_trailing_space = true;
        });
        let output = clipboard_session(&app);
        let text = "Первое слово\nSecond line 🦀\n\n  exact whitespace  ";
        deliver(output, &app, text.into()).expect("clipboard delivery");
        assert_eq!(app.effects().clipboard_writes, [text]);
        assert!(app.effects().paste_calls.is_empty());
        assert!(app.effects().notices.is_empty());
        assert!(take(&app).is_none());
    }

    #[test]
    fn cancellation_invalidates_output_already_owned_by_recognition() {
        let app = AppHandle::default();
        let output = clipboard_session(&app);
        cancel(&app);
        deliver(output, &app, "cancelled result".into()).expect("cancel is a no-op");
        assert_eq!(app.effects(), Effects::default());
    }

    #[test]
    fn previous_output_cannot_write_after_another_recording_begins() {
        let app = AppHandle::default();
        let old = clipboard_session(&app);
        let current = clipboard_session(&app);
        deliver(old, &app, "stale old text".into()).expect("stale no-op");
        deliver(current, &app, "current text".into()).expect("current output");
        assert_eq!(app.effects().clipboard_writes, ["current text"]);
        assert!(app.effects().paste_calls.is_empty());
    }

    #[test]
    fn missing_session_recovers_full_text_in_clipboard() {
        let app = AppHandle::default();
        let text = "recover this complete result\nincluding its tail";
        deliver(None, &app, text.into()).expect("fallback clipboard");
        assert_eq!(app.effects().clipboard_writes, [text]);
        assert!(app.effects().paste_calls.is_empty());
    }

    #[test]
    fn failed_clipboard_write_returns_error_and_never_claims_success() {
        let app = AppHandle::default();
        let output = clipboard_session(&app);
        app.fail_clipboard();
        assert!(deliver(output, &app, "preserve me".into()).is_err());
        let effects = app.effects();
        assert!(effects.clipboard_writes.is_empty());
        assert!(effects.paste_calls.is_empty());
        assert_eq!(effects.notices, ["clipboard_error"]);
    }
}

#[cfg(all(windows, test))]
#[path = "../dictation-target/native_fixture.rs"]
mod native_fixture;

#[cfg(all(windows, test))]
mod windows_output;
