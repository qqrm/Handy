//! Hosted-VM-only desktop check. Never run this on a working desktop.
//! Only the fixture's own session is passed to the production controller.
use super::*;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING};
use windows::Foundation::Uri;
use windows::Media::{
    Core::MediaSource,
    Playback::{MediaPlayer, MediaPlayerFailedEventArgs},
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

struct Fixture {
    player: MediaPlayer,
    path: std::path::PathBuf,
    failure: Arc<Mutex<Option<String>>>,
    failure_token: i64,
}
impl Fixture {
    fn describe(&self, stage: &str) {
        let failure = self.failure.lock().unwrap().clone();
        match self.player.PlaybackSession() {
            Ok(playback) => eprintln!(
                "Fixture stage={stage}: playback_state={:?}, position_100ns={:?}, failure={failure:?}",
                playback.PlaybackState(),
                playback.Position().map(|position| position.Duration)
            ),
            Err(error) => eprintln!("Fixture stage={stage}: PlaybackSession error={error:?}"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.player.RemoveMediaFailed(self.failure_token);
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
fn wait_state(fixture: &Fixture, session: &dyn MediaSession, expected: PlaybackState, stage: &str) {
    fixture.describe(stage);
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.state() != Ok(expected) {
        if Instant::now() >= deadline {
            fixture.describe(stage);
            panic!(
                "Fixture did not reach {expected:?} at {stage}; SMTC state={:?}",
                session.state()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    fixture.describe(&format!("{stage} complete"));
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Windows SMTC job"]
fn isolated_windows_session_pause_resume_and_user_override() {
    require_isolated_smtc_test_environment();
    let _apartment = Apartment::new().expect("WinRT MTA");
    unsafe { SetCurrentProcessExplicitAppUserModelID(w!("Handy.IsolatedSmtcTest")) }.unwrap();
    let player = MediaPlayer::new().unwrap();
    let failure = Arc::new(Mutex::new(None));
    let observed_failure = failure.clone();
    let failure_token = player
        .MediaFailed(
            &TypedEventHandler::<MediaPlayer, MediaPlayerFailedEventArgs>::new(move |_, args| {
                let details = match args.as_ref() {
                    Some(args) => format!(
                        "category={:?}, HRESULT={:?}, message={:?}",
                        args.Error(),
                        args.ExtendedErrorCode()
                            .map(|code| format!("0x{:08X}", code.0 as u32)),
                        args.ErrorMessage()
                    ),
                    None => "MediaFailed event without arguments".into(),
                };
                eprintln!("Fixture MediaFailed: {details}");
                *observed_failure.lock().unwrap() = Some(details);
                Ok(())
            }),
        )
        .unwrap();
    let path = std::env::temp_dir().join(format!("handy-smtc-test-{}.wav", std::process::id()));
    let fixture = Fixture {
        player,
        path,
        failure,
        failure_token,
    };
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
    fixture.describe("initial Play requested, before production controller");
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
        if Instant::now() >= deadline {
            fixture.describe("own SMTC session discovery timed out");
            panic!("This desktop did not publish the isolated MediaPlayer SMTC session");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let session: Arc<dyn MediaSession> = Arc::new(WindowsSession { manager, session });
    wait_state(
        &fixture,
        session.as_ref(),
        PlaybackState::Playing,
        "initial fixture playback, before production controller",
    );
    let controller = MediaPauseController::with_backend(Arc::new(OnlyFixture(session.clone())));
    controller.begin(1);
    controller.apply(1, true, true);
    controller.flush();
    wait_state(
        &fixture,
        session.as_ref(),
        PlaybackState::Paused,
        "first controller pause",
    );
    controller.remove();
    controller.flush();
    wait_state(
        &fixture,
        session.as_ref(),
        PlaybackState::Playing,
        "controller restore",
    );
    controller.begin(2);
    controller.apply(2, true, true);
    controller.flush();
    wait_state(
        &fixture,
        session.as_ref(),
        PlaybackState::Paused,
        "second controller pause",
    );
    fixture.player.Play().unwrap();
    wait_state(
        &fixture,
        session.as_ref(),
        PlaybackState::Playing,
        "user Play override",
    );
    std::thread::sleep(Duration::from_millis(250));
    fixture.player.Pause().unwrap();
    wait_state(
        &fixture,
        session.as_ref(),
        PlaybackState::Paused,
        "user Pause override",
    );
    controller.remove();
    controller.flush();
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    println!("Own silent session: pause/resume and observed user Play→Pause passed; other sessions were not controlled");
}
