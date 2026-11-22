//! Per-recording output ownership. A stop command fixes the destination before
//! recognition; only that recording may deliver the resulting text.

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod policy;

pub use policy::OutputMode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

#[cfg(windows)]
use crate::dictation_target::{DeliveryReceipt, TargetCapture, TargetError, TargetSnapshot};
#[cfg(windows)]
use crate::settings::{AutoSubmitKey, ClipboardHandling, DictationTarget, PasteMethod};

#[derive(Default)]
pub struct DictationOutputState(Mutex<OutputState>);

#[derive(Default)]
struct OutputState {
    active: Option<Arc<AtomicBool>>,
    recording: Option<RecordingOutput>,
    prepared: Option<PreparedOutput>,
}

struct RecordingOutput {
    active: Arc<AtomicBool>,
    #[cfg(windows)]
    config: OutputConfig,
    #[cfg(windows)]
    target: policy::RecordingTarget<TargetCapture>,
}

/// Owned by the asynchronous transcription task, never by the next recording.
pub struct PreparedOutput {
    active: Arc<AtomicBool>,
    mode: OutputMode,
    #[cfg(windows)]
    config: OutputConfig,
    #[cfg(windows)]
    target: Option<TargetCapture>,
}

#[cfg(windows)]
struct OutputConfig {
    append_trailing_space: bool,
    paste_method: PasteMethod,
    clipboard_handling: ClipboardHandling,
    auto_submit: bool,
    auto_submit_key: AutoSubmitKey,
    notify_delivery_failure: bool,
    paste_delay_ms: u64,
    paste_delay_after_ms: u64,
}

/// Called before recording UI/model work can change the focused control.
pub fn begin(app: &AppHandle) {
    cancel(app);
    let Some(state) = app.try_state::<DictationOutputState>() else {
        return;
    };
    let active = Arc::new(AtomicBool::new(true));
    #[cfg(windows)]
    let settings = crate::settings::get_settings(app);
    let recording = RecordingOutput {
        active: Arc::clone(&active),
        #[cfg(windows)]
        config: OutputConfig {
            append_trailing_space: settings.append_trailing_space,
            paste_method: settings.paste_method,
            clipboard_handling: settings.clipboard_handling,
            auto_submit: settings.auto_submit,
            auto_submit_key: settings.auto_submit_key,
            notify_delivery_failure: settings.notify_delivery_failure,
            paste_delay_ms: settings.paste_delay_ms,
            paste_delay_after_ms: settings.paste_delay_after_ms,
        },
        #[cfg(windows)]
        target: policy::RecordingTarget::begin(
            match settings.dictation_target {
                DictationTarget::RecordingStart => policy::CaptureMoment::Start,
                DictationTarget::RecordingEnd => policy::CaptureMoment::End,
            },
            TargetCapture::begin,
        ),
    };
    match state.0.lock() {
        Ok(mut state) => {
            state.active = Some(active);
            state.recording = Some(recording);
        }
        Err(_) => {
            log::error!("Dictation output state is unavailable; output will stay in clipboard")
        }
    };
}

/// Runs at the accepted stop edge, before microphone tail capture or ASR.
pub fn prepare_stop(app: &AppHandle, mode: OutputMode) {
    let Some(state) = app.try_state::<DictationOutputState>() else {
        return;
    };
    let recording = state
        .0
        .lock()
        .ok()
        .and_then(|mut state| state.recording.take());
    let Some(recording) = recording else { return };
    let prepared = PreparedOutput {
        active: recording.active,
        mode,
        #[cfg(windows)]
        config: recording.config,
        #[cfg(windows)]
        target: recording.target.finish(mode, TargetCapture::begin),
    };
    // Capture only pins the native anchor and starts its worker here. Waiting
    // for an accessibility provider belongs to delivery, never microphone stop.
    if let Ok(mut state) = state.0.lock() {
        if prepared.active.load(Ordering::Acquire)
            && state
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &prepared.active))
        {
            state.prepared = Some(prepared);
        }
    };
}

pub fn take(app: &AppHandle) -> Option<PreparedOutput> {
    let state = app.try_state::<DictationOutputState>()?;
    let output = state.0.lock().ok()?.prepared.take();
    output
}

/// Invalidate also the output already owned by an ASR/delivery task.
pub fn cancel(app: &AppHandle) {
    let Some(state) = app.try_state::<DictationOutputState>() else {
        return;
    };
    if let Ok(mut state) = state.0.lock() {
        if let Some(active) = state.active.take() {
            active.store(false, Ordering::Release);
        }
        state.recording = None;
        state.prepared = None;
    };
}

fn copy_to_clipboard(app: &AppHandle, text: &str) -> Result<(), String> {
    crate::clipboard::write_text_to_clipboard(app, text).inspect_err(|_| {
        crate::notifications::show_clipboard_error(app);
    })
}

/// A missing session cannot authorize insertion into whatever happens to be
/// focused. Clipboard writes use the normal clipboard, including Win+V history.
pub fn deliver(
    output: Option<PreparedOutput>,
    app: &AppHandle,
    text: String,
) -> Result<(), String> {
    let Some(output) = output else {
        return copy_to_clipboard(app, &text);
    };
    if !output.active.load(Ordering::Acquire) {
        return Ok(());
    }
    if output.mode == OutputMode::ClipboardOnly {
        return copy_to_clipboard(app, &text);
    }
    #[cfg(windows)]
    {
        deliver_to_target(output, app, text)
    }
    #[cfg(not(windows))]
    {
        crate::utils::paste(text, app.clone())
    }
}

#[cfg(windows)]
fn deliver_to_target(
    mut output: PreparedOutput,
    app: &AppHandle,
    text: String,
) -> Result<(), String> {
    let config = &output.config;
    let text = if config.append_trailing_space {
        format!("{text} ")
    } else {
        text
    };
    if config.paste_method == PasteMethod::None {
        return if config.clipboard_handling == ClipboardHandling::CopyToClipboard {
            copy_to_clipboard(app, &text)
        } else {
            Ok(())
        };
    }
    let mut target = output.target.take().map(TargetCapture::resolve);
    if !output.active.load(Ordering::Acquire) {
        return Ok(());
    }
    let result = match target.as_mut() {
        Some(Ok(target)) if target.requires_foreground_paste() => {
            deliver_to_foreground_target(target, &output.active, config, app, &text)?
        }
        Some(Ok(target)) => target.insert_if_active(&text, Arc::clone(&output.active)),
        Some(Err(error)) => Err(*error),
        None => Err(TargetError::Unavailable),
    };
    if !output.active.load(Ordering::Acquire) {
        return Ok(());
    }
    match result {
        Ok(receipt) if receipt.verified => {
            if config.clipboard_handling == ClipboardHandling::CopyToClipboard {
                copy_to_clipboard(app, &text)?;
            }
            if config.auto_submit {
                let focused = matches!(target.as_ref(), Some(Ok(target)) if target.is_focused_target() == Ok(true));
                let submitted = focused
                    && match send_submit_batch(config.auto_submit_key, &output.active) {
                        Ok(sent) => sent,
                        Err(error) => {
                            log::warn!("{error}");
                            false
                        }
                    };
                if !submitted && config.notify_delivery_failure {
                    crate::notifications::show_auto_submit_skipped(app);
                }
            }
            Ok(())
        }
        result => {
            let error = result.err().unwrap_or(TargetError::Uncertain);
            // Never retry an uncertain native write: the original field may
            // already contain the text. Keep the complete result recoverable.
            copy_to_clipboard(app, &text)?;
            if config.notify_delivery_failure {
                use crate::notifications::DeliveryFailureReason;
                let reason = match error {
                    TargetError::Changed | TargetError::AlreadyUsed => {
                        DeliveryFailureReason::Changed
                    }
                    TargetError::Unsupported
                    | TargetError::Protected
                    | TargetError::InvalidText => DeliveryFailureReason::Unsupported,
                    TargetError::Uncertain => DeliveryFailureReason::Uncertain,
                    TargetError::Unavailable | TargetError::Busy => {
                        DeliveryFailureReason::Unavailable
                    }
                };
                crate::notifications::show_delivery_fallback(app, reason);
            }
            log::warn!("Dictation retained in clipboard: {error}");
            Ok(())
        }
    }
}

/// Some browser editors expose a readable selection but no background writer.
/// They can use their normal paste command only while the captured field AND
/// selection remain current. Confirmation still reads the exact original field.
#[cfg(windows)]
fn deliver_to_foreground_target(
    target: &mut TargetSnapshot,
    active: &Arc<AtomicBool>,
    config: &OutputConfig,
    app: &AppHandle,
    text: &str,
) -> Result<Result<DeliveryReceipt, TargetError>, String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    if config.paste_method == PasteMethod::ExternalScript {
        // An arbitrary external script has no target-addressing contract.
        return Ok(Err(TargetError::Unsupported));
    }
    std::thread::sleep(std::time::Duration::from_millis(config.paste_delay_ms));
    if !active.load(Ordering::Acquire) {
        return Ok(Err(TargetError::Changed));
    }
    let clipboard = crate::clipboard::prepare_guarded_paste_clipboard(
        app,
        text,
        config.clipboard_handling == ClipboardHandling::DontModify,
    )
    .inspect_err(|_| crate::notifications::show_clipboard_error(app))?;
    let result = (|| {
        target.prepare_foreground_insert(text, Arc::clone(active))?;
        if !target.foreground_insert_is_ready()? || !clipboard.is_current() {
            return Err(TargetError::Changed);
        }
        let (key, modifiers) = match config.paste_method {
            PasteMethod::CtrlShiftV => (VK_V, vec![VK_CONTROL, VK_SHIFT]),
            PasteMethod::ShiftInsert => (VK_INSERT, vec![VK_SHIFT]),
            _ => (VK_V, vec![VK_CONTROL]),
        };
        match send_key_batch(key, &modifiers, active) {
            Ok(true) => {}
            Ok(false) => return Err(TargetError::Busy),
            Err(error) => {
                log::warn!("{error}");
                return Err(TargetError::Uncertain);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(
            config.paste_delay_after_ms,
        ));
        target.confirm_foreground_insert()
    })();
    if matches!(result, Ok(receipt) if receipt.verified) {
        if let Err(error) = clipboard.restore_after_verified_paste(app) {
            // Insertion is already verified. Never repeat it for a restoration
            // failure, and leave the copied dictation available to the user.
            log::warn!("Could not restore clipboard after verified dictation: {error}");
        }
    }
    Ok(result)
}

/// Keep the existing active-field auto-submit behavior, with a fresh exact
/// target check immediately before this batch. Never submit background delivery.
/// Windows has no atomic compare-focus-and-SendInput API: a programmatic focus
/// change during this final interval remains a documented best-effort limit.
#[cfg(windows)]
fn send_submit_batch(key: AutoSubmitKey, active: &AtomicBool) -> Result<bool, String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let modifiers = match key {
        AutoSubmitKey::Enter => vec![],
        AutoSubmitKey::CtrlEnter => vec![VK_CONTROL],
        AutoSubmitKey::CmdEnter => vec![VK_LWIN],
    };
    send_key_batch(VK_RETURN, &modifiers, active)
}

#[cfg(windows)]
fn send_key_batch(
    key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
    modifiers: &[windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY],
    active: &AtomicBool,
) -> Result<bool, String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    // Never synthesize releases for modifiers the user is physically holding.
    if [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
        .iter()
        .any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) } < 0)
        || !active.load(Ordering::Acquire)
    {
        return Ok(false);
    }
    let event = |key: u16, released| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(key),
                dwFlags: if released {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                ..Default::default()
            },
        },
    };
    let mut plan: Vec<_> = modifiers.iter().map(|key| (key.0, false)).collect();
    plan.push((key.0, false));
    plan.push((key.0, true));
    plan.extend(modifiers.iter().rev().map(|key| (key.0, true)));
    let inputs: Vec<_> = plan
        .iter()
        .map(|&(key, released)| event(key, released))
        .collect();
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        // Do not replay the command after a partial native write. Only release
        // the keys that the accepted prefix actually left logically pressed.
        let releases: Vec<_> = policy::keys_to_release(&plan, sent as usize)
            .into_iter()
            .map(|key| event(key, true))
            .collect();
        if !releases.is_empty()
            && unsafe { SendInput(&releases, std::mem::size_of::<INPUT>() as i32) }
                != releases.len() as u32
        {
            log::warn!("Windows rejected cleanup of an incomplete keyboard batch");
        }
        return Err("Windows did not accept the complete dictation keyboard batch".into());
    }
    Ok(true)
}
