#[path = "media_control_worker.rs"]
mod worker;
use worker::{PlaybackBackend, PlaybackWorker};

#[cfg(any(target_os = "windows", test))]
#[path = "media_control_windows_batch.rs"]
mod windows_batch;

#[cfg(target_os = "windows")]
#[path = "media_control_windows.rs"]
mod platform_windows;

#[cfg(target_os = "linux")]
#[path = "media_control_linux.rs"]
mod platform_linux;

/// Keeps the existing platform behavior on a recording-scoped worker. Calls
/// made by the output controller only update a request; they never wait on SMTC.
#[derive(Clone)]
pub(crate) struct MediaControlManager(PlaybackWorker);

#[derive(Clone, Debug, PartialEq, Eq)]
enum PausedPlaybackState {
    #[cfg(target_os = "macos")]
    Global,
    #[cfg(any(target_os = "linux", test))]
    Session(String),
    #[cfg(target_os = "windows")]
    Sessions(platform_windows::PausedSessions),
}

struct PlatformMediaControlBackend;

impl PlaybackBackend for PlatformMediaControlBackend {
    type Paused = PausedPlaybackState;

    fn pause_playback(&mut self) -> Result<Option<Self::Paused>, String> {
        platform_pause_playback()
    }

    fn resume_playback(&mut self, paused: Self::Paused) -> Result<(), String> {
        platform_resume_playback(paused)
    }
}

impl MediaControlManager {
    pub(crate) fn new() -> Self {
        Self(PlaybackWorker::new(PlatformMediaControlBackend))
    }

    pub(crate) fn begin(&self, generation: u64, resume: bool) {
        self.0.begin(generation, resume);
    }

    pub(crate) fn apply(&self, generation: u64) {
        self.0.apply(generation);
    }

    pub(crate) fn remove(&self) {
        self.0.remove();
    }

    pub(crate) fn restore_before_exit(&self) {
        self.0.restore_before_exit();
    }
}

#[cfg(target_os = "macos")]
fn platform_pause_playback() -> Result<Option<PausedPlaybackState>, String> {
    // Direct MediaRemote state calls from Handy are entitlement-gated on newer macOS releases.
    // Host the tiny state adapter inside Apple's Perl process so this remains a native private
    // MediaRemote query without going through osascript/JXA.
    if !crate::media_remote::private_is_playing()? {
        return Ok(None);
    }

    crate::media_remote::play_pause_key()?;
    Ok(Some(PausedPlaybackState::Global))
}

#[cfg(target_os = "macos")]
fn platform_resume_playback(paused_playback: PausedPlaybackState) -> Result<(), String> {
    match paused_playback {
        PausedPlaybackState::Global => {}
        #[cfg(test)]
        PausedPlaybackState::Session(_) => return Ok(()),
    }

    crate::media_remote::play_pause_key()
}

#[cfg(target_os = "windows")]
fn platform_pause_playback() -> Result<Option<PausedPlaybackState>, String> {
    platform_windows::pause_active_sessions()
        .map(|sessions| (!sessions.is_empty()).then(|| PausedPlaybackState::Sessions(sessions)))
}

#[cfg(target_os = "windows")]
fn platform_resume_playback(paused_playback: PausedPlaybackState) -> Result<(), String> {
    match paused_playback {
        PausedPlaybackState::Sessions(sessions) => platform_windows::resume_sessions(sessions),
        #[cfg(test)]
        _ => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn platform_pause_playback() -> Result<Option<PausedPlaybackState>, String> {
    platform_linux::pause_active_session().map(|session| session.map(PausedPlaybackState::Session))
}

#[cfg(target_os = "linux")]
fn platform_resume_playback(paused_playback: PausedPlaybackState) -> Result<(), String> {
    let PausedPlaybackState::Session(service_name) = paused_playback;
    platform_linux::resume_session(&service_name)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(target_os = "macos")]
    fn music_player_state() -> Option<String> {
        let output = std::process::Command::new("osascript")
            .args(["-e", "tell application \"Music\" to player state as string"])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[cfg(target_os = "macos")]
    fn pause_music_for_live_test() {
        let _ = std::process::Command::new("osascript")
            .args(["-e", "tell application \"Music\" to pause"])
            .status();
        std::thread::sleep(Duration::from_millis(750));
    }

    #[test]
    #[ignore = "requires macOS Music and live system media controls"]
    #[cfg(target_os = "macos")]
    fn macos_live_pause_does_not_start_media_when_nothing_is_playing() {
        pause_music_for_live_test();

        let before = music_player_state();
        assert_ne!(before.as_deref(), Some("playing"));

        let paused = platform_pause_playback().expect("macOS media pause precheck failed");

        std::thread::sleep(Duration::from_millis(750));
        let after = music_player_state();

        assert_eq!(paused, None);
        assert_ne!(after.as_deref(), Some("playing"));
    }
}
