//! Actual VLC/MPRIS integration, only inside the explicit hosted private bus.
//! The environment checks prevent accidental use; the VM and private bus supply
//! isolation. Never spoof these variables or run this on a working desktop.
use super::*;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn require_isolated_mpris_test_environment() {
    for (name, expected) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Linux"),
        ("HANDY_RUN_ISOLATED_MPRIS_TESTS", "1"),
        ("HANDY_MPRIS_PRIVATE_BUS", "1"),
    ] {
        assert!(
            std::env::var(name).is_ok_and(|value| value == expected),
            "Real MPRIS tests require the isolated GitHub-hosted Linux job: \
             {name} must be {expected}. Never run on a working desktop or spoof these values."
        );
    }
    assert!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .is_ok_and(|address| address.starts_with("unix:")),
        "Run the hosted fixture under dbus-run-session with a private Unix bus"
    );
}

struct Fixture {
    child: Child,
    directory: PathBuf,
}

impl Fixture {
    fn spawn(label: &str) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "handy-mpris-{}-{label}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let audio = directory.join("fixture.wav");
        write_wav(&audio);
        let log = std::fs::File::create(directory.join("vlc.log")).unwrap();
        let child = Command::new("vlc")
            .args([
                "--ignore-config",
                "--intf=dummy",
                "--extraintf=dbus",
                "--aout=dummy",
                "--no-video",
                "--no-one-instance",
                "--no-media-library",
                "--no-metadata-network-access",
                "--no-loop",
                "--no-repeat",
            ])
            .arg(&audio)
            .env("HOME", &directory)
            .env("XDG_CONFIG_HOME", &directory)
            .env("XDG_CACHE_HOME", &directory)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("Launch distro VLC on the hosted VM");
        Self { child, directory }
    }

    fn session(&mut self) -> Arc<Session> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(self.child.try_wait().unwrap().is_none(), "VLC exited");
            for session in enumerate().expect("Private D-Bus enumeration") {
                let pid = bounded(READ_TIMEOUT, async {
                    let dbus = zbus::fdo::DBusProxy::new(&session.connection)
                        .await
                        .map_err(error)?;
                    dbus.get_connection_unix_process_id(
                        BusName::try_from(session.owner.as_str()).map_err(error)?,
                    )
                    .await
                    .map_err(error)
                });
                if pid == Ok(self.child.id()) {
                    eprintln!(
                        "Fixture PID={} service={} owner={}",
                        self.child.id(), session.service, session.owner
                    );
                    return session;
                }
            }
            assert!(Instant::now() < deadline, "VLC MPRIS session not found");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(log) = std::fs::read_to_string(self.directory.join("vlc.log")) {
            let tail: String = log.chars().rev().take(4000).collect();
            eprintln!("VLC diagnostic tail: {}", tail.chars().rev().collect::<String>());
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn write_wav(path: &std::path::Path) {
    // A real PCM stream decoded by VLC; audio is discarded by VLC's own dummy
    // output module. We assert real clock movement, not audible hardware output.
    let rate = 16_000_u32;
    let data_bytes = rate * 120 * 2;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16_u32.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&rate.to_le_bytes()).unwrap();
    file.write_all(&(rate * 2).to_le_bytes()).unwrap();
    file.write_all(&2_u16.to_le_bytes()).unwrap();
    file.write_all(&16_u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_bytes.to_le_bytes()).unwrap();
    file.write_all(&vec![0; data_bytes as usize]).unwrap();
}

struct OnlyFixtures(Vec<Arc<Session>>);

impl MediaBackend for OnlyFixtures {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        Ok(self.0.iter().cloned().map(|session| session as Arc<dyn MediaSession>).collect())
    }
}

fn wait_state(session: &Session, expected: PlaybackState, stage: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual = session.state();
        if actual == Ok(expected) {
            return;
        }
        assert!(Instant::now() < deadline, "{stage}: expected {expected:?}, got {actual:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn complete(mut operation: MediaOperation) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = operation() {
            assert_eq!(result, Ok(true));
            return;
        }
        assert!(Instant::now() < deadline, "MPRIS operation did not acknowledge");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn position(session: &Session) -> i64 {
    bounded(READ_TIMEOUT, async {
        session.proxy().await.map_err(error)?.get_property::<i64>("Position").await.map_err(error)
    }).expect("Read actual VLC position")
}

fn require_progress(session: &Session, stage: &str) {
    wait_state(session, PlaybackState::Playing, stage);
    let before = position(session);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let after = position(session);
        if after >= before + 100_000 {
            eprintln!("Real VLC progress at {stage}: position_us={before}->{after}");
            return;
        }
        assert!(Instant::now() < deadline, "VLC clock did not progress at {stage}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn require_paused_clock(session: &Session, stage: &str) {
    wait_state(session, PlaybackState::Paused, stage);
    // Allow a final decoder tick to settle before testing the paused clock.
    std::thread::sleep(Duration::from_millis(100));
    let before = position(session);
    std::thread::sleep(Duration::from_millis(300));
    let after = position(session);
    assert!((after - before).abs() <= 10_000, "VLC clock moved while paused at {stage}: {before}->{after}");
    eprintln!("Real VLC paused at {stage}: position_us={before}->{after}");
}

fn start(controller: &MediaPauseController, generation: u64) {
    controller.begin(generation);
    controller.apply(generation, true, true);
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Linux MPRIS job"]
fn isolated_linux_vlc_pause_resume_override_and_replacement() {
    require_isolated_mpris_test_environment();
    assert!(enumerate().unwrap().is_empty(), "The private fixture bus must start without players");
    let mut first = Fixture::spawn("first");
    let a = first.session();
    assert_eq!(a.service, "org.mpris.MediaPlayer2.vlc");
    require_progress(&a, "initial first");
    let mut second = Fixture::spawn("second");
    let b = second.session();
    require_progress(&b, "initial second");
    complete(b.pause().unwrap());
    require_paused_clock(&b, "initially paused player");

    let controller = MediaPauseController::with_backend(Arc::new(OnlyFixtures(vec![a.clone(), b.clone()])));
    start(&controller, 1);
    require_paused_clock(&a, "recording");
    controller.remove();
    require_progress(&a, "recording ended");
    require_paused_clock(&b, "initially paused player remains paused");

    complete(b.play().unwrap());
    require_progress(&b, "second playing");
    start(&controller, 2);
    require_paused_clock(&a, "both players recording first");
    require_paused_clock(&b, "both players recording second");
    controller.remove();
    require_progress(&a, "both players restored first");
    require_progress(&b, "both players restored second");
    complete(b.pause().unwrap());
    require_paused_clock(&b, "second manually paused");

    start(&controller, 3);
    require_paused_clock(&a, "before user override");
    complete(a.play().unwrap());
    require_progress(&a, "user resumed");
    complete(a.pause().unwrap());
    require_paused_clock(&a, "user paused again");
    controller.remove();
    std::thread::sleep(Duration::from_millis(200));
    require_paused_clock(&a, "user pause preserved after recording");

    complete(a.play().unwrap());
    require_progress(&a, "before replacement");
    start(&controller, 4);
    require_paused_clock(&a, "old owner recording");
    first.stop();
    let mut replacement = Fixture::spawn("replacement");
    let c = replacement.session();
    assert_eq!(a.service, c.service, "Exercise well-known-name reuse");
    assert_ne!(a.owner, c.owner, "Replacement must have a different D-Bus connection");
    assert_eq!(a.is_live(), Ok(false));
    complete(c.pause().unwrap());
    require_paused_clock(&c, "replacement manually paused");
    controller.remove();
    std::thread::sleep(Duration::from_millis(200));
    require_paused_clock(&c, "replacement not resumed by old lease");
    controller.restore_before_exit();
}

#[test]
fn mpris_playback_status_rejects_unknown_values() {
    assert_eq!(playback_state("Playing"), Ok(PlaybackState::Playing));
    assert_eq!(playback_state("Paused"), Ok(PlaybackState::Paused));
    assert_eq!(playback_state("Stopped"), Ok(PlaybackState::Other));
    assert!(playback_state("playing").is_err());
    assert!(playback_state("").is_err());
}

// These protocol tests supplement the real VLC test. They do not substitute a
// simulated player for proof that a real decoder's clock pauses and resumes.
struct ProtocolState {
    status: AtomicU64,
    pause_started: mpsc::Sender<()>,
    pause_gate: Mutex<Option<oneshot::Receiver<()>>>,
    pause_calls: AtomicU64,
    play_calls: AtomicU64,
}

struct ProtocolPlayer(Arc<ProtocolState>);

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl ProtocolPlayer {
    #[zbus(property)]
    fn playback_status(&self) -> &str {
        match self.0.status.load(Ordering::SeqCst) {
            0 => "Playing",
            1 => "Paused",
            _ => "Stopped",
        }
    }

    #[zbus(property)]
    fn can_control(&self) -> bool { true }

    #[zbus(property)]
    fn can_pause(&self) -> bool { true }

    #[zbus(property)]
    fn can_play(&self) -> bool { true }

    async fn pause(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.0.pause_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.0.pause_gate.lock().unwrap().take();
        let _ = self.0.pause_started.send(());
        if let Some(gate) = gate {
            gate.await.map_err(|_| zbus::fdo::Error::Failed("Fixture gate dropped".into()))?;
        }
        self.0.status.store(1, Ordering::SeqCst);
        self.playback_status_changed(&emitter).await?;
        Ok(())
    }

    async fn play(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.0.play_calls.fetch_add(1, Ordering::SeqCst);
        self.0.status.store(0, Ordering::SeqCst);
        self.playback_status_changed(&emitter).await?;
        Ok(())
    }
}

fn emit_status(connection: &Connection, state: &ProtocolState, value: u64) {
    state.status.store(value, Ordering::SeqCst);
    let value = match value { 0 => "Playing", 1 => "Paused", _ => "Stopped" };
    bounded(READ_TIMEOUT, async {
        connection.emit_signal(
            None::<&str>,
            PATH,
            PROPERTIES,
            "PropertiesChanged",
            &(PLAYER, HashMap::from([("PlaybackStatus", zbus::zvariant::Value::from(value))]), Vec::<String>::new()),
        ).await.map_err(error)
    }).unwrap();
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted Linux MPRIS job"]
fn isolated_linux_dbus_delayed_ack_and_signal_ownership() {
    require_isolated_mpris_test_environment();
    let (pause_started, started) = mpsc::channel();
    let (release, gate) = oneshot::channel();
    let state = Arc::new(ProtocolState {
        status: AtomicU64::new(0),
        pause_started,
        pause_gate: Mutex::new(Some(gate)),
        pause_calls: AtomicU64::new(0),
        play_calls: AtomicU64::new(0),
    });
    let service = "org.mpris.MediaPlayer2.HandyProtocolFixture";
    let connection = bounded(SETUP_TIMEOUT, async {
        zbus::connection::Builder::session().map_err(error)?
            .name(service).map_err(error)?
            .serve_at(PATH, ProtocolPlayer(state.clone())).map_err(error)?
            .build().await.map_err(error)
    }).unwrap();
    let session = enumerate().unwrap().into_iter().find(|session| session.service == service).unwrap();
    let controller = MediaPauseController::with_backend(Arc::new(OnlyFixtures(vec![session.clone()])));
    start(&controller, 101);
    started.recv_timeout(Duration::from_secs(3)).expect("Pause reached the service");
    controller.remove();
    // Exceed the read deadline while the dispatched Pause has no reply. The
    // lease must survive End; a later Paused transition must still be restored.
    std::thread::sleep(Duration::from_millis(750));
    assert_eq!(state.status.load(Ordering::SeqCst), 0);
    assert_eq!(state.play_calls.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.play_calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "Late Pause was not restored after End");
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_state(&session, PlaybackState::Playing, "late acknowledgement restoration");
    assert_eq!(state.pause_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.play_calls.load(Ordering::SeqCst), 1);
    controller.restore_before_exit();
    drop(controller);

    let ownership = Arc::new(PauseOwnership::default());
    let watch = session.watch(ownership.clone()).unwrap();
    emit_status(&connection, &state, 1);
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    assert!(ownership.can_resume());
    // Both raw events must be retained even when Get sees only the final Pause.
    emit_status(&connection, &state, 0);
    emit_status(&connection, &state, 1);
    assert_eq!(session.state(), Ok(PlaybackState::Paused));
    assert!(!ownership.can_resume(), "Queued user Play -> Pause lost by observer");
    drop(watch);

    let old_owner = session.owner.clone();
    bounded(SETUP_TIMEOUT, async { connection.close().await.map_err(error) }).unwrap();
    let replacement = bounded(SETUP_TIMEOUT, async {
        zbus::connection::Builder::session().map_err(error)?
            .name(service).map_err(error)?
            .serve_at(PATH, ProtocolPlayer(state.clone())).map_err(error)?
            .build().await.map_err(error)
    }).unwrap();
    let replacement_session = enumerate().unwrap().into_iter().find(|session| session.service == service).unwrap();
    assert_ne!(old_owner, replacement_session.owner);
    assert_eq!(session.is_live(), Ok(false));
    let mut rejected = session.play().unwrap();
    assert_eq!(rejected(), Some(Ok(false)), "Old owner must never target replacement");
    assert_eq!(replacement_session.state(), Ok(PlaybackState::Paused));
    assert_eq!(state.play_calls.load(Ordering::SeqCst), 1);
    bounded(SETUP_TIMEOUT, async { replacement.close().await.map_err(error) }).unwrap();
}
