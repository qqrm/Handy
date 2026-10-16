//! Hosted-VM-only desktop check. Never run this on a working desktop.
//! Only the fixture's own session is passed to the production controller.
use super::*;
use std::time::{Duration, Instant};
use windows::core::{w, IInspectable, HSTRING};
use windows::Foundation::{TimeSpan, Uri};
use windows::Media::{
    Core::MediaSource,
    Editing::{MediaClip, MediaComposition},
    MediaProperties::{AudioEncodingProperties, MediaEncodingProfile, VideoEncodingQuality},
    Playback::{MediaPlaybackState, MediaPlayer, MediaPlayerFailedEventArgs},
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::UI::Color;

struct Fixture {
    player: MediaPlayer,
    path: Option<std::path::PathBuf>,
    failure: Arc<Mutex<Option<String>>>,
    failure_token: i64,
    frames: Arc<AtomicU64>,
    frame_token: Option<i64>,
    // The composition must live as long as its generated stream.
    composition: Option<MediaComposition>,
}
impl Fixture {
    fn describe(&self, stage: &str) {
        let failure = self.failure.lock().unwrap().clone();
        match self.player.PlaybackSession() {
            Ok(playback) => eprintln!(
                "Fixture stage={stage}: playback_state={:?}, position_100ns={:?}, frames={}, failure={failure:?}",
                playback.PlaybackState(),
                playback.Position().map(|position| position.Duration),
                self.frames.load(Ordering::SeqCst)
            ),
            Err(error) => eprintln!("Fixture stage={stage}: PlaybackSession error={error:?}"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.player.RemoveMediaFailed(self.failure_token);
        if let Some(token) = self.frame_token {
            let _ = self.player.RemoveVideoFrameAvailable(token);
        }
        let _ = self.player.Close();
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
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

fn require_progress(fixture: &Fixture, stage: &str) {
    let playback = fixture.player.PlaybackSession().unwrap();
    let initial_position = playback.Position().unwrap().Duration;
    let initial_frames = fixture.frames.load(Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            fixture.failure.lock().unwrap().is_none(),
            "MediaFailed at {stage}"
        );
        let position = playback.Position().unwrap().Duration;
        let frames = fixture.frames.load(Ordering::SeqCst);
        if playback.PlaybackState().unwrap() == MediaPlaybackState::Playing
            && position >= initial_position + 1_000_000
            && (fixture.composition.is_none() || frames > initial_frames)
        {
            eprintln!("Real playback progress at {stage}: position_100ns={initial_position}->{position}, frames={initial_frames}->{frames}");
            return;
        }
        if Instant::now() >= deadline {
            fixture.describe(stage);
            panic!("Real playback clock/frame progression missing at {stage}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn require_paused_clock(fixture: &Fixture, stage: &str) {
    let playback = fixture.player.PlaybackSession().unwrap();
    assert_eq!(
        playback.PlaybackState().unwrap(),
        MediaPlaybackState::Paused
    );
    let before = playback.Position().unwrap().Duration;
    std::thread::sleep(Duration::from_millis(250));
    let after = playback.Position().unwrap().Duration;
    assert_eq!(
        before, after,
        "Playback clock moved while paused at {stage}"
    );
    assert!(
        fixture.failure.lock().unwrap().is_none(),
        "MediaFailed at {stage}"
    );
    eprintln!("Real playback clock paused at {stage}: position_100ns={before}->{after}");
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Windows SMTC job"]
fn isolated_windows_session_pause_resume_and_user_override() {
    require_isolated_smtc_test_environment();
    run_fixture(false);
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Windows SMTC job"]
fn isolated_windows_video_session_pause_resume_and_user_override() {
    require_isolated_smtc_test_environment();
    run_fixture(true);
}

fn run_fixture(video_only: bool) {
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
    let mut fixture = Fixture {
        player,
        path: None,
        failure,
        failure_token,
        frames: Arc::new(AtomicU64::new(0)),
        frame_token: None,
        composition: None,
    };
    fixture.player.SetVolume(0.0).unwrap();
    fixture.player.SetIsLoopingEnabled(true).unwrap();
    if video_only {
        set_video_source(&mut fixture);
    } else {
        set_audio_source(&mut fixture);
    }
    exercise_controller(&fixture);
}

fn set_video_source(fixture: &mut Fixture) {
    // Built-in Windows video source: no audio track, external codec or driver.
    let composition = MediaComposition::new().unwrap();
    let clip = MediaClip::CreateFromColor(
        Color {
            A: 255,
            R: 0,
            G: 0,
            B: 0,
        },
        TimeSpan {
            Duration: 300_000_000,
        },
    )
    .unwrap();
    composition.Clips().unwrap().Append(&clip).unwrap();
    let profile = MediaEncodingProfile::CreateMp4(VideoEncodingQuality::Wvga).unwrap();
    profile.SetAudio(None::<&AudioEncodingProperties>).unwrap();
    let video = profile.Video().unwrap();
    video.SetWidth(64).unwrap();
    video.SetHeight(64).unwrap();
    video.FrameRate().unwrap().SetNumerator(10).unwrap();
    video.FrameRate().unwrap().SetDenominator(1).unwrap();
    let stream = composition
        .GenerateMediaStreamSourceWithProfile(&profile)
        .unwrap();
    let frames = fixture.frames.clone();
    fixture.frame_token = Some(
        fixture
            .player
            .VideoFrameAvailable(&TypedEventHandler::<MediaPlayer, IInspectable>::new(
                move |_, _| {
                    frames.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            ))
            .unwrap(),
    );
    fixture.player.SetIsVideoFrameServerEnabled(true).unwrap();
    fixture
        .player
        .SetSource(&MediaSource::CreateFromMediaStreamSource(&stream).unwrap())
        .unwrap();
    fixture.composition = Some(composition);
    eprintln!("Video-only fixture: 64x64 at 10fps, no audio track, built-in Windows composition/frame server");
}

fn set_audio_source(fixture: &mut Fixture) {
    let path = std::env::temp_dir().join(format!("handy-smtc-test-{}.wav", std::process::id()));
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
    std::fs::write(&path, wave).unwrap();
    let uri = Uri::CreateUri(&HSTRING::from(format!(
        "file:///{}",
        path.to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20")
    )))
    .unwrap();
    fixture
        .player
        .SetSource(&MediaSource::CreateFromUri(&uri).unwrap())
        .unwrap();
    fixture.path = Some(path);
    eprintln!("Audio-only fixture: silent PCM WAV, requires a Windows audio render endpoint");
}

fn exercise_controller(fixture: &Fixture) {
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
        fixture,
        session.as_ref(),
        PlaybackState::Playing,
        "initial fixture playback, before production controller",
    );
    require_progress(fixture, "initial playback before production controller");
    let controller = MediaPauseController::with_backend(Arc::new(OnlyFixture(session.clone())));
    controller.begin(1);
    controller.apply(1, true, true);
    controller.flush();
    wait_state(
        fixture,
        session.as_ref(),
        PlaybackState::Paused,
        "first controller pause",
    );
    require_paused_clock(fixture, "first controller pause");
    controller.remove();
    controller.flush();
    wait_state(
        fixture,
        session.as_ref(),
        PlaybackState::Playing,
        "controller restore",
    );
    require_progress(fixture, "controller restore");
    controller.begin(2);
    controller.apply(2, true, true);
    controller.flush();
    wait_state(
        fixture,
        session.as_ref(),
        PlaybackState::Paused,
        "second controller pause",
    );
    require_paused_clock(fixture, "second controller pause");
    fixture.player.Play().unwrap();
    wait_state(
        fixture,
        session.as_ref(),
        PlaybackState::Playing,
        "user Play override",
    );
    std::thread::sleep(Duration::from_millis(250));
    fixture.player.Pause().unwrap();
    wait_state(
        fixture,
        session.as_ref(),
        PlaybackState::Paused,
        "user Pause override",
    );
    controller.remove();
    controller.flush();
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    require_paused_clock(fixture, "user override retained after recording");
    println!("Own real session: pause/resume and observed user Play→Pause passed; other sessions were not controlled");
}
