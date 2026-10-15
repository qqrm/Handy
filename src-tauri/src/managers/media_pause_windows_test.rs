//! Explicitly opted-in desktop check. Only the fixture's own session is passed
//! to the production controller; other sessions are never paused or resumed.
use super::*;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING};
use windows::Foundation::Uri;
use windows::Media::{Core::MediaSource, Playback::MediaPlayer};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

struct Fixture {
    player: MediaPlayer,
    path: std::path::PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.player.Close();
        let _ = std::fs::remove_file(&self.path);
    }
}
struct OnlyFixture(Arc<dyn MediaSession>);
impl MediaBackend for OnlyFixture {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        Ok(vec![self.0.clone()])
    }
}
fn wait_state(session: &dyn MediaSession, expected: PlaybackState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.state() != Ok(expected) {
        assert!(
            Instant::now() < deadline,
            "Fixture did not reach {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "Explicit opt-in: publishes a silent, isolated MediaPlayer SMTC session"]
fn isolated_windows_session_pause_resume_and_user_override() {
    let _apartment = Apartment::new().expect("WinRT MTA");
    unsafe { SetCurrentProcessExplicitAppUserModelID(w!("Handy.IsolatedSmtcTest")) }.unwrap();
    let player = MediaPlayer::new().unwrap();
    let path = std::env::temp_dir().join(format!("handy-smtc-test-{}.wav", std::process::id()));
    let fixture = Fixture { player, path };
    let data_len = 8_000u32 * 2 * 10;
    let mut wave = Vec::new();
    wave.extend_from_slice(b"RIFF");
    wave.extend_from_slice(&(36 + data_len).to_le_bytes());
    wave.extend_from_slice(b"WAVEfmt ");
    wave.extend_from_slice(&16u32.to_le_bytes());
    wave.extend_from_slice(&1u16.to_le_bytes());
    wave.extend_from_slice(&1u16.to_le_bytes());
    wave.extend_from_slice(&8_000u32.to_le_bytes());
    wave.extend_from_slice(&16_000u32.to_le_bytes());
    wave.extend_from_slice(&2u16.to_le_bytes());
    wave.extend_from_slice(&16u16.to_le_bytes());
    wave.extend_from_slice(b"data");
    wave.extend_from_slice(&data_len.to_le_bytes());
    wave.resize(wave.len() + data_len as usize, 0);
    std::fs::write(&fixture.path, wave).unwrap();
    fixture.player.SetVolume(0.0).unwrap();
    fixture.player.SetIsLoopingEnabled(true).unwrap();
    let uri = Uri::CreateUri(&HSTRING::from(format!(
        "file:///{}",
        fixture
            .path
            .to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20")
    )))
    .unwrap();
    fixture
        .player
        .SetSource(&MediaSource::CreateFromUri(&uri).unwrap())
        .unwrap();
    fixture.player.Play().unwrap();
    let manager = SessionManager::RequestAsync().unwrap().get().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let session = loop {
        let own = manager.GetSessions().unwrap().into_iter().find(|session| {
            session
                .SourceAppUserModelId()
                .map(|id| id == "Handy.IsolatedSmtcTest")
                .unwrap_or(false)
        });
        if let Some(session) = own {
            break session;
        }
        assert!(
            Instant::now() < deadline,
            "This desktop did not publish the isolated MediaPlayer SMTC session"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let session: Arc<dyn MediaSession> = Arc::new(WindowsSession { manager, session });
    wait_state(session.as_ref(), PlaybackState::Playing);
    let controller = MediaPauseController::with_backend(Arc::new(OnlyFixture(session.clone())));
    controller.begin(1);
    controller.apply(1, true, true);
    controller.flush();
    wait_state(session.as_ref(), PlaybackState::Paused);
    controller.remove();
    controller.flush();
    wait_state(session.as_ref(), PlaybackState::Playing);
    controller.begin(2);
    controller.apply(2, true, true);
    controller.flush();
    wait_state(session.as_ref(), PlaybackState::Paused);
    fixture.player.Play().unwrap();
    wait_state(session.as_ref(), PlaybackState::Playing);
    std::thread::sleep(Duration::from_millis(250));
    fixture.player.Pause().unwrap();
    wait_state(session.as_ref(), PlaybackState::Paused);
    controller.remove();
    controller.flush();
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    println!("Own silent session: pause/resume and observed user Play→Pause passed; other sessions were not controlled");
}
