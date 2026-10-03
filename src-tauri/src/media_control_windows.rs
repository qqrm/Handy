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

/// Pauses every currently-playing session instead of just one: with more than
/// one source (e.g. YouTube in the browser and Spotify), pausing only the
/// current session leaves the rest playing straight into the microphone.
///
/// Returns the source app user model IDs of the sessions *we* paused. A
/// session that cannot be paused (no pause control, declined the request,
/// vanished mid-enumeration) is skipped so one misbehaving app cannot leave
/// the others unpaused or, worse, paused without being tracked for resume.
pub fn pause_active_sessions() -> Result<Vec<String>, String> {
    let _guard = WinRtGuard::initialize()?;
    let manager = request_manager()?;
    let sessions = enumerate_sessions(&manager)?;

    let mut paused_ids = Vec::new();
    for session in sessions {
        if !session_is_playing(&session)? {
            continue;
        }

        let Some(playback_info) = playback_info(&session)? else {
            continue;
        };
        let controls = match playback_info.Controls() {
            Ok(controls) => controls,
            Err(err) => {
                debug!("Skipping Windows media session without playback controls: {err}");
                continue;
            }
        };

        let pause_enabled = match controls.IsPauseEnabled() {
            Ok(enabled) => enabled,
            Err(err) => {
                debug!("Failed to query Windows pause support: {err}");
                continue;
            }
        };
        if !pause_enabled {
            continue;
        }

        let paused = match session.TryPauseAsync().and_then(|op| op.get()) {
            Ok(paused) => paused,
            Err(err) => {
                debug!("Failed to request Windows pause: {err}");
                continue;
            }
        };
        if !paused {
            continue;
        }

        let Some(source_app_user_model_id) = session_source_app_user_model_id(&session)? else {
            debug!("Skipped storing paused Windows session because its source app id disappeared");
            continue;
        };
        // The same app can own several sessions; resuming is keyed by id, so
        // storing duplicates would only make the resume pass double-report.
        if !paused_ids.contains(&source_app_user_model_id) {
            paused_ids.push(source_app_user_model_id);
        }
    }

    Ok(paused_ids)
}

/// Resumes exactly the sessions identified by `source_app_user_model_ids`.
/// Sessions that exited while paused are skipped; sessions the user resumed
/// (or paused) on their own are left alone — a play request on an
/// already-playing session is avoided by the status check below.
pub fn resume_sessions(source_app_user_model_ids: &[String]) -> Result<(), String> {
    let _guard = WinRtGuard::initialize()?;
    let manager = request_manager()?;
    let sessions = enumerate_sessions(&manager)?;

    for session in sessions {
        let Some(source_app_user_model_id) = session_source_app_user_model_id(&session)? else {
            continue;
        };
        if !source_app_user_model_ids.contains(&source_app_user_model_id) {
            continue;
        }

        let Some(playback_info) = playback_info(&session)? else {
            continue;
        };
        let status = playback_info
            .PlaybackStatus()
            .map_err(|err| format!("Failed to query Windows playback status: {err}"))?;

        if status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing {
            continue;
        }

        let controls = playback_info
            .Controls()
            .map_err(|err| format!("Failed to get Windows playback controls: {err}"))?;

        let play_enabled = controls
            .IsPlayEnabled()
            .map_err(|err| format!("Failed to query Windows play support: {err}"))?;
        if !play_enabled {
            debug!(
                "Skipping Windows media resume because session '{}' no longer supports play",
                source_app_user_model_id
            );
            continue;
        }

        let resumed = session
            .TryPlayAsync()
            .map_err(|err| format!("Failed to request Windows play: {err}"))?
            .get()
            .map_err(|err| format!("Failed to wait for Windows play: {err}"))?;

        if !resumed {
            debug!(
                "Windows media session '{}' declined the play request; ignoring",
                source_app_user_model_id
            );
        }
    }

    Ok(())
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
        let session = sessions.GetAt(index).map_err(|err| {
            format!("Failed to read Windows media session at index {index}: {err}")
        })?;
        collected.push(session);
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

fn session_is_playing(session: &GlobalSystemMediaTransportControlsSession) -> Result<bool, String> {
    let Some(playback_info) = playback_info(session)? else {
        return Ok(false);
    };

    let status = playback_info
        .PlaybackStatus()
        .map_err(|err| format!("Failed to query Windows playback status: {err}"))?;

    Ok(status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
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
