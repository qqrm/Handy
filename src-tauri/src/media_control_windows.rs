use super::windows_batch::{self, MediaSession, PlaybackStatus};
use log::debug;
use windows::core::HRESULT;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

struct WinRtGuard {
    should_uninitialize: bool,
}

impl WinRtGuard {
    fn initialize() -> Result<Self, String> {
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self {
                should_uninitialize: true,
            }),
            Err(err) if err.code() == RPC_E_CHANGED_MODE => Ok(Self {
                should_uninitialize: false,
            }),
            Err(err) => Err(format!("Failed to initialize WinRT: {err}")),
        }
    }
}

impl Drop for WinRtGuard {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe { RoUninitialize() };
        }
    }
}

/// Pauses playing sessions that expose a usable source app ID. Per-session
/// failures are skipped; every successful pause is returned for later resume.
/// Setup errors can only be returned before any pause has been attempted.
pub fn pause_active_sessions() -> Result<Vec<String>, String> {
    let _guard = WinRtGuard::initialize()?;
    let manager = request_manager()?;
    let sessions = enumerate_sessions(&manager)?;
    Ok(windows_batch::pause_sessions(&sessions))
}

/// Attempts to resume every tracked source app that is still paused. Errors
/// from individual sessions are reported after the remaining sessions are tried.
/// Already-playing, stopped, closed, and untracked sessions are left alone.
pub fn resume_sessions(source_app_user_model_ids: &[String]) -> Result<(), String> {
    if source_app_user_model_ids.is_empty() {
        return Ok(());
    }
    let _guard = WinRtGuard::initialize()?;
    let manager = request_manager()?;
    let sessions = enumerate_sessions(&manager)?;
    windows_batch::resume_sessions(&sessions, source_app_user_model_ids)
}

impl MediaSession for GlobalSystemMediaTransportControlsSession {
    fn source_app_id(&self) -> Result<Option<String>, String> {
        session_source_app_user_model_id(self)
    }

    fn playback_status(&self) -> Result<Option<PlaybackStatus>, String> {
        let Some(info) = playback_info(self)? else {
            return Ok(None);
        };
        let status = info
            .PlaybackStatus()
            .map_err(|err| format!("Failed to query Windows playback status: {err}"))?;
        Ok(Some(match status {
            GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => {
                PlaybackStatus::Playing
            }
            GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => {
                PlaybackStatus::Paused
            }
            _ => PlaybackStatus::Other,
        }))
    }

    fn pause_enabled(&self) -> Result<bool, String> {
        let Some(info) = playback_info(self)? else {
            return Ok(false);
        };
        info.Controls()
            .map_err(|err| format!("Failed to get Windows playback controls: {err}"))?
            .IsPauseEnabled()
            .map_err(|err| format!("Failed to query Windows pause support: {err}"))
    }

    fn play_enabled(&self) -> Result<bool, String> {
        let Some(info) = playback_info(self)? else {
            return Ok(false);
        };
        info.Controls()
            .map_err(|err| format!("Failed to get Windows playback controls: {err}"))?
            .IsPlayEnabled()
            .map_err(|err| format!("Failed to query Windows play support: {err}"))
    }

    fn try_pause(&self) -> Result<bool, String> {
        self.TryPauseAsync()
            .map_err(|err| format!("Failed to request Windows pause: {err}"))?
            .get()
            .map_err(|err| format!("Failed to wait for Windows pause: {err}"))
    }

    fn try_play(&self) -> Result<bool, String> {
        self.TryPlayAsync()
            .map_err(|err| format!("Failed to request Windows play: {err}"))?
            .get()
            .map_err(|err| format!("Failed to wait for Windows play: {err}"))
    }
}

fn request_manager() -> Result<GlobalSystemMediaTransportControlsSessionManager, String> {
    GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .map_err(|err| format!("Failed to request Windows media session manager: {err}"))?
        .get()
        .map_err(|err| format!("Failed to wait for Windows media session manager: {err}"))
}

fn enumerate_sessions(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Result<Vec<GlobalSystemMediaTransportControlsSession>, String> {
    let sessions = manager
        .GetSessions()
        .map_err(|err| format!("Failed to enumerate Windows media sessions: {err}"))?;
    let count = sessions
        .Size()
        .map_err(|err| format!("Failed to query Windows media session count: {err}"))?;

    let mut collected = Vec::with_capacity(count as usize);
    for index in 0..count {
        match sessions.GetAt(index) {
            Ok(session) => collected.push(session),
            Err(err) => debug!("Skipping unreadable Windows media session at index {index}: {err}"),
        }
    }

    Ok(collected)
}

fn playback_info(
    session: &GlobalSystemMediaTransportControlsSession,
) -> Result<
    Option<windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackInfo>,
    String,
> {
    match session.GetPlaybackInfo() {
        Ok(info) => Ok(Some(info)),
        Err(err) if session_missing_error(err.code()) => Ok(None),
        Err(err) => Err(format!("Failed to get Windows playback info: {err}")),
    }
}

fn session_source_app_user_model_id(
    session: &GlobalSystemMediaTransportControlsSession,
) -> Result<Option<String>, String> {
    match session.SourceAppUserModelId() {
        Ok(value) => Ok(Some(value.to_string())),
        Err(err) if session_missing_error(err.code()) => Ok(None),
        Err(err) => Err(format!("Failed to query Windows source app id: {err}")),
    }
}

fn session_missing_error(code: HRESULT) -> bool {
    code == HRESULT(0x80070490u32 as i32) || code == HRESULT(0x80004005u32 as i32)
}
