//! Hosted-VM-only desktop check. Never run this on a working desktop.
//! Only the fixture's own session is passed to the production controller.
use super::super::worker::{PlaybackBackend, PlaybackWorker};
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, IInspectable};
use windows::Foundation::{TimeSpan, TypedEventHandler};
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
struct OnlyFixture(Vec<GlobalSystemMediaTransportControlsSession>);
impl PlaybackBackend for OnlyFixture {
    type Paused = Vec<GlobalSystemMediaTransportControlsSession>;

    fn pause_playback(&mut self) -> Result<Option<Self::Paused>, String> {
        let paused = windows_batch::pause_sessions(&self.0);
        Ok((!paused.is_empty()).then_some(paused))
    }

    fn resume_playback(&mut self, paused: Self::Paused) -> Result<(), String> {
        windows_batch::resume_sessions(&paused)
    }
}

fn wait_state(
    fixture: &Fixture,
    session: &GlobalSystemMediaTransportControlsSession,
    expected: PlaybackStatus,
    stage: &str,
) {
    fixture.describe(stage);
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.playback_status() != Ok(Some(expected)) {
        if Instant::now() >= deadline {
            fixture.describe(stage);
            panic!("Fixture did not reach {expected:?} at {stage}");
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

fn require_isolated_smtc_test_environment() {
    for (name, expected) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Windows"),
        ("HANDY_RUN_ISOLATED_SMTC_TESTS", "1"),
    ] {
        assert!(
            std::env::var(name).is_ok_and(|value| value == expected),
            "Native SMTC tests require an isolated GitHub-hosted Windows VM: \
             {name} must be {expected}. Do not spoof these values on a working desktop."
        );
    }
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Windows SMTC job"]
fn isolated_windows_video_pause_resume_and_legacy_policy() {
    require_isolated_smtc_test_environment();
    run_fixture();
}

fn run_fixture() {
    let _apartment = WinRtGuard::initialize().expect("WinRT MTA");
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
    set_video_source(&mut fixture);
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

fn exercise_controller(fixture: &Fixture) {
    fixture.player.Play().unwrap();
    fixture.describe("initial Play requested, before production controller");
    let manager = request_manager().unwrap();
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
    wait_state(
        fixture,
        &session,
        PlaybackStatus::Playing,
        "initial playback",
    );
    require_progress(fixture, "initial playback before production worker");
    let controller = PlaybackWorker::new(OnlyFixture(vec![session.clone()]));
    controller.begin(1, true);
    controller.apply(1);
    controller.flush();
    wait_state(
        fixture,
        &session,
        PlaybackStatus::Paused,
        "controller pause",
    );
    require_paused_clock(fixture, "controller pause");
    controller.remove();
    controller.flush();
    wait_state(
        fixture,
        &session,
        PlaybackStatus::Playing,
        "controller restore",
    );
    require_progress(fixture, "controller restore");

    fixture.player.Pause().unwrap();
    wait_state(
        fixture,
        &session,
        PlaybackStatus::Paused,
        "initially paused",
    );
    controller.begin(2, true);
    controller.apply(2);
    controller.flush();
    controller.remove();
    controller.flush();
    require_paused_clock(fixture, "initially paused playback stays unowned");

    fixture.player.Play().unwrap();
    wait_state(
        fixture,
        &session,
        PlaybackStatus::Playing,
        "second playback",
    );
    require_progress(fixture, "second playback");
    controller.begin(3, false);
    controller.apply(3);
    controller.flush();
    wait_state(
        fixture,
        &session,
        PlaybackStatus::Paused,
        "pause without auto-resume",
    );
    controller.remove();
    controller.flush();
    require_paused_clock(fixture, "play_after_recording=false preserved");
    println!("Own real SMTC session: pause/resume, initially paused, and no-auto-resume passed; other sessions were not controlled");
}
