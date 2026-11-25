//! Real PulseAudio sink integration in a private, explicitly opted-in hosted
//! fixture. No physical audio modules or inherited desktop servers are used.
//! Never spoof the guards or run this fixture on a working desktop.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SINK_A: &str = "handy_output_mute_a";
const SINK_B: &str = "handy_output_mute_b";

fn require_isolated_environment() {
    for (name, expected) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Linux"),
        ("HANDY_RUN_ISOLATED_OUTPUT_MUTE_TESTS", "1"),
    ] {
        assert!(
            std::env::var(name).is_ok_and(|value| value == expected),
            "Real output mute tests require the isolated GitHub-hosted Linux job: \
             {name} must be {expected}. Never spoof these values or use a working desktop."
        );
    }
}

#[derive(Clone)]
struct PrivatePulseCommands {
    directory: PathBuf,
    server: String,
}

impl Commands for PrivatePulseCommands {
    fn command(&self, program: &str) -> Result<Command, String> {
        // The production fallback path is exercised, but it can never run a
        // system PipeWire/ALSA command, even if the private server fails.
        if program != "pactl" {
            return Err(format!("Private Pulse fixture refuses {program}"));
        }
        let mut command = Command::new(program);
        command
            .env("PULSE_SERVER", &self.server)
            .env("PULSE_RUNTIME_PATH", &self.directory)
            .env("PULSE_CLIENTCONFIG", self.directory.join("client.conf"))
            .env("XDG_RUNTIME_DIR", &self.directory)
            .env("XDG_CONFIG_HOME", &self.directory)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null());
        Ok(command)
    }
}

struct Fixture {
    child: Child,
    commands: PrivatePulseCommands,
}

impl Fixture {
    fn spawn() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("handy-output-mute-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Pulse's configuration syntax separates module arguments by spaces.
        assert!(!directory.to_string_lossy().chars().any(char::is_whitespace));
        let socket = directory.join("native.sock");
        let config = directory.join("fixture.pa");
        std::fs::write(
            &config,
            format!(
                "load-module module-native-protocol-unix socket={} auth-anonymous=1\n",
                socket.display()
            ),
        )
        .unwrap();
        std::fs::write(directory.join("client.conf"), "autospawn = no\n").unwrap();
        let log = std::fs::File::create(directory.join("pulseaudio.log")).unwrap();
        let child = Command::new("pulseaudio")
            .args([
                "--daemonize=no",
                "--use-pid-file=no",
                "--exit-idle-time=-1",
                "--disable-shm=yes",
                "--realtime=no",
                "--high-priority=no",
                "--log-target=stderr",
                "-n",
                "--file",
            ])
            .arg(&config)
            .env("PULSE_RUNTIME_PATH", &directory)
            .env("PULSE_STATE_PATH", &directory)
            .env("XDG_RUNTIME_DIR", &directory)
            .env("XDG_CONFIG_HOME", &directory)
            .env_remove("PULSE_SERVER")
            .env_remove("PULSE_SCRIPT")
            .env_remove("PULSE_CONFIG")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("Launch a private PulseAudio daemon without default modules");
        let mut fixture = Self {
            child,
            commands: PrivatePulseCommands {
                server: format!("unix:{}", socket.display()),
                directory,
            },
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "Private PulseAudio exited"
            );
            if fixture.pactl(&["info"]).is_ok() {
                return fixture;
            }
            assert!(
                Instant::now() < deadline,
                "Private PulseAudio did not become ready"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn backend(&self) -> Backend<PrivatePulseCommands> {
        Backend {
            commands: self.commands.clone(),
        }
    }

    fn pactl(&self, args: &[&str]) -> Result<String, String> {
        self.backend().run("pactl", args)
    }

    fn assert_state(
        &self,
        stage: &str,
        original_muted: Option<bool>,
        new_muted: bool,
        default: &str,
    ) {
        // Inspect real protocol responses independently of the product's mute
        // parser. A faulty product parser cannot make this assertion pass.
        let original = self.pactl(&["get-sink-mute", SINK_A]);
        match original_muted {
            Some(true) => assert_eq!(original.unwrap().trim(), "Mute: yes"),
            Some(false) => assert_eq!(original.unwrap().trim(), "Mute: no"),
            None => assert!(original.is_err(), "The original sink must have disappeared"),
        }
        assert_eq!(
            self.pactl(&["get-sink-mute", SINK_B]).unwrap().trim(),
            if new_muted { "Mute: yes" } else { "Mute: no" }
        );
        assert_eq!(self.pactl(&["get-default-sink"]).unwrap().trim(), default);
        let original_json = original_muted.map_or_else(|| "null".into(), |value| value.to_string());
        eprintln!(
            "{{\"fixture\":\"private-pulseaudio\",\"stage\":\"{stage}\",\"original_sink\":\"{SINK_A}\",\"original_muted\":{original_json},\"other_sink\":\"{SINK_B}\",\"other_muted\":{new_muted},\"default\":\"{default}\"}}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            if let Ok(log) = std::fs::read_to_string(self.commands.directory.join("pulseaudio.log"))
            {
                let tail: String = log.chars().rev().take(4000).collect();
                eprintln!(
                    "Private PulseAudio diagnostic tail: {}",
                    tail.chars().rev().collect::<String>()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&self.commands.directory);
    }
}

#[test]
#[ignore = "Restricted to the isolated GitHub-hosted private PulseAudio job"]
fn isolated_pulseaudio_mute_restores_original_sink() {
    require_isolated_environment();
    let fixture = Fixture::spawn();
    let module_a = fixture
        .pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK_A}"),
        ])
        .unwrap();
    fixture
        .pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK_B}"),
        ])
        .unwrap();
    fixture.pactl(&["set-default-sink", SINK_A]).unwrap();
    fixture.pactl(&["set-sink-mute", SINK_A, "0"]).unwrap();
    fixture.pactl(&["set-sink-mute", SINK_B, "1"]).unwrap();
    fixture.assert_state("before", Some(false), true, SINK_A);

    let mut lease = super::super::mute_with_backend(fixture.backend())
        .unwrap()
        .unwrap();
    fixture.assert_state("muted", Some(true), true, SINK_A);
    fixture.pactl(&["set-default-sink", SINK_B]).unwrap();
    lease.restore().unwrap();
    fixture.assert_state(
        "restored_original_after_default_switch",
        Some(false),
        true,
        SINK_B,
    );

    // The new default was muted by its owner. Recording must not acquire it
    // or later unmute it, and the earlier lease must also be idempotent.
    assert!(super::super::mute_with_backend(fixture.backend())
        .unwrap()
        .is_none());
    lease.restore().unwrap();
    fixture.assert_state("already_muted_is_unowned", Some(false), true, SINK_B);

    fixture.pactl(&["set-default-sink", SINK_A]).unwrap();
    let mut lease = super::super::mute_with_backend(fixture.backend())
        .unwrap()
        .unwrap();
    fixture.pactl(&["set-default-sink", SINK_B]).unwrap();
    fixture.pactl(&["unload-module", module_a.trim()]).unwrap();
    assert!(
        lease.restore().is_err(),
        "Restoration must not retarget the new default"
    );
    fixture.assert_state(
        "missing_original_does_not_unmute_new_default",
        None,
        true,
        SINK_B,
    );
}
