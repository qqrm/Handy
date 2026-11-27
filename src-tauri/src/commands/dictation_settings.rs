//! Preferences for one dictation's destination and recording reminders.

use crate::settings::{self, DictationTarget};
use tauri::AppHandle;

#[tauri::command]
#[specta::specta]
pub fn change_dictation_target_setting(
    app: AppHandle,
    target: DictationTarget,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.dictation_target = target;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_notify_delivery_failure_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.notify_delivery_failure = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_recording_reminders_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.recording_reminders_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_recording_reminder_minutes_setting(
    app: AppHandle,
    minutes: u32,
) -> Result<(), String> {
    settings::validate_recording_reminder_minutes(minutes)?;
    let mut settings = settings::get_settings(&app);
    settings.recording_reminder_minutes = minutes;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_recording_reminder_sound_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.recording_reminder_sound = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}
