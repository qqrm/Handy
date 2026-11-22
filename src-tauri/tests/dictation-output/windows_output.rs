//! Windows tests use real desktop Edit controls and the unchanged UIA/native
//! target implementation. Clipboard/settings/notifications are external fakes.

use crate::dictation_output::{begin, deliver, prepare_stop, take, OutputMode};
use crate::native_fixture::{Action, Fixture};
use crate::settings::{ClipboardHandling, DictationTarget};
use crate::AppHandle;

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn output_resolves_pending_capture_and_uses_frozen_settings() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusA(7, 7));
    let app = AppHandle::default();
    app.configure(|settings| {
        settings.dictation_target = DictationTarget::RecordingStart;
        settings.clipboard_handling = ClipboardHandling::CopyToClipboard;
        settings.append_trailing_space = true;
        settings.auto_submit = false;
    });
    begin(&app);
    app.configure(|settings| {
        settings.dictation_target = DictationTarget::RecordingEnd;
        settings.clipboard_handling = ClipboardHandling::DontModify;
        settings.append_trailing_space = false;
        settings.auto_submit = true;
        settings.notify_delivery_failure = true;
    });
    // Keep the target stable while acquisition runs. This is the mandatory
    // positive delivery case, not an assertion that a capture race must win.
    prepare_stop(&app, OutputMode::Insert);
    let output = take(&app);
    deliver(output, &app, "dictation".into()).expect("addressed delivery");
    assert_eq!(fixture.call(Action::ReadA), "before dictation after");
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    let effects = app.effects();
    assert_eq!(effects.clipboard_writes, ["dictation "]);
    assert!(effects.paste_calls.is_empty());
    assert!(
        effects.notices.is_empty(),
        "auto-submit setting was frozen at Start"
    );
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn pending_start_capture_never_follows_the_later_stop_focus() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusA(7, 7));
    let app = AppHandle::default();
    begin(&app);
    app.configure(|settings| settings.dictation_target = DictationTarget::RecordingEnd);
    fixture.call(Action::FocusB);
    prepare_stop(&app, OutputMode::Insert);
    let output = take(&app);
    deliver(output, &app, "start target ".into()).expect("safe capture result");
    // If acquisition finished before the focus change, A receives the text.
    // If focus changed while the provider was still acquiring, full clipboard
    // fallback is correct. Neither outcome may acquire or write B at Stop/ASR.
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    let original = fixture.call(Action::ReadA);
    if original == "before start target after" {
        assert!(app.effects().clipboard_writes.is_empty());
    } else {
        assert_eq!(original, "before after");
        assert_eq!(app.effects().clipboard_writes, ["start target "]);
    }
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn output_captures_end_destination_before_recognition_only() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusB);
    let app = AppHandle::default();
    app.configure(|settings| settings.dictation_target = DictationTarget::RecordingEnd);
    begin(&app);
    fixture.call(Action::FocusA(7, 7));
    prepare_stop(&app, OutputMode::Insert);
    let output = take(&app);
    fixture.call(Action::FocusB);
    deliver(output, &app, "end target ".into()).expect("captured stop destination");
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    let original = fixture.call(Action::ReadA);
    if original == "before end target after" {
        assert!(app.effects().clipboard_writes.is_empty());
    } else {
        assert_eq!(original, "before after");
        assert_eq!(app.effects().clipboard_writes, ["end target "]);
    }
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn destroyed_target_keeps_the_complete_text_in_clipboard() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusA(7, 7));
    let app = AppHandle::default();
    app.configure(|settings| {
        settings.auto_submit = true;
        settings.notify_delivery_failure = true;
    });
    begin(&app);
    prepare_stop(&app, OutputMode::Insert);
    let output = take(&app);
    fixture.call(Action::DestroyA);
    fixture.call(Action::FocusB);
    let text = "Complete dictated text\nincluding the final line 🦀";
    deliver(output, &app, text.into()).expect("clipboard fallback");
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    assert_eq!(app.effects().clipboard_writes, [text]);
    let notices = app.effects().notices;
    assert!(
        notices == ["fallback:Unavailable"] || notices == ["fallback:Changed"],
        "destruction during/after capture must report a recoverable unavailable/changed target: {notices:?}"
    );
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn clipboard_stop_does_not_insert_or_submit_to_any_control() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusA(7, 7));
    let app = AppHandle::default();
    app.configure(|settings| {
        settings.auto_submit = true;
        settings.dictation_target = DictationTarget::RecordingEnd;
    });
    begin(&app);
    fixture.call(Action::FocusB);
    prepare_stop(&app, OutputMode::ClipboardOnly);
    let output = take(&app);
    deliver(output, &app, "clipboard only".into()).expect("clipboard completion");
    assert_eq!(fixture.call(Action::ReadA), "before after");
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    assert_eq!(app.effects().clipboard_writes, ["clipboard only"]);
    assert!(app.effects().notices.is_empty());
}
