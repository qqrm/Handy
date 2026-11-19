use super::OutputBackend;
use std::process::Command;

pub(super) struct Backend<R = SystemCommands> {
    commands: R,
}

pub(super) struct SystemCommands;

pub(super) trait Commands: Send + 'static {
    fn command(&self, program: &str) -> Result<Command, String>;
}

impl Commands for SystemCommands {
    fn command(&self, program: &str) -> Result<Command, String> {
        Ok(Command::new(program))
    }
}

pub(super) fn backend() -> Backend {
    Backend {
        commands: SystemCommands,
    }
}

pub(super) enum Target {
    PipeWire { id: u32, serial: u64 },
    PulseAudio { name: String },
    Alsa { card: String },
}

type Discover<B> = fn(&B) -> Result<Target, String>;

impl<R: Commands> OutputBackend for Backend<R> {
    type Target = Target;

    fn default_target(&self) -> Result<Target, String> {
        // Fallback is limited to read-only discovery. Once a backend changes
        // an output, its lease never falls back to a different output/backend.
        let candidates: [Discover<Self>; 3] =
            [Self::pipewire_target, Self::pulse_target, Self::alsa_target];
        let mut errors = Vec::new();
        for discover in candidates {
            match discover(self).and_then(|target| self.is_muted(&target).map(|_| target)) {
                Ok(target) => return Ok(target),
                Err(error) => errors.push(error),
            }
        }
        Err(format!(
            "Cannot identify a readable output mute control: {}",
            errors.join("; ")
        ))
    }

    fn is_muted(&self, target: &Target) -> Result<bool, String> {
        match target {
            Target::PipeWire { id, serial } => {
                self.verify_pipewire_target(*id, *serial)?;
                parse_pipewire_mute(&self.run("wpctl", &["get-volume", &id.to_string()])?)
            }
            Target::PulseAudio { name } => {
                parse_pulse_mute(&self.run("pactl", &["get-sink-mute", name])?)
            }
            Target::Alsa { card } => {
                parse_alsa_mute(&self.run("amixer", &["-c", card, "get", "Master"])?)
            }
        }
    }

    fn set_muted(&self, target: &Target, muted: bool) -> Result<(), String> {
        let value = if muted { "1" } else { "0" };
        match target {
            Target::PipeWire { id, serial } => {
                // PipeWire IDs can be reused after a node disappears. Refuse a
                // replacement, even when it happens to reuse the original ID.
                self.verify_pipewire_target(*id, *serial)?;
                self.run("wpctl", &["set-mute", &id.to_string(), value])?;
            }
            Target::PulseAudio { name } => {
                self.run("pactl", &["set-sink-mute", name, value])?;
            }
            Target::Alsa { card } => {
                self.run(
                    "amixer",
                    &[
                        "-c",
                        card,
                        "set",
                        "Master",
                        if muted { "mute" } else { "unmute" },
                    ],
                )?;
            }
        }
        Ok(())
    }
}

impl<R: Commands> Backend<R> {
    fn pipewire_target(&self) -> Result<Target, String> {
        parse_pipewire_target(&self.run("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"])?)
    }

    fn pulse_target(&self) -> Result<Target, String> {
        let name = self.run("pactl", &["get-default-sink"])?;
        let name = name.trim();
        validate_identifier(name)?;
        Ok(Target::PulseAudio { name: name.into() })
    }

    fn alsa_target(&self) -> Result<Target, String> {
        let card = parse_alsa_card(&self.run("amixer", &["info"])?)?;
        // A virtual "default" plugin is not necessarily an ALSA card.
        // Only accept a card that can be reopened by its actual ID.
        let resolved = parse_alsa_card(&self.run("amixer", &["-c", &card, "info"])?)?;
        if resolved != card {
            return Err("ALSA default did not resolve to the same card ID".into());
        }
        Ok(Target::Alsa { card })
    }

    fn verify_pipewire_target(&self, id: u32, serial: u64) -> Result<(), String> {
        match parse_pipewire_target(&self.run("wpctl", &["inspect", &id.to_string()])?)? {
            Target::PipeWire {
                id: current_id,
                serial: current_serial,
            } if id == current_id && serial == current_serial => Ok(()),
            _ => Err("The original PipeWire output node no longer exists".into()),
        }
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let output = self
            .commands
            .command(program)?
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .map_err(|error| format!("Cannot run {program}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "{program} {} failed ({}): {}",
                args.first().copied().unwrap_or_default(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| format!("Cannot decode {program} output: {error}"))
    }
}

fn validate_identifier(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with(['-', '@'])
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        Err("Audio backend returned an invalid output identifier".into())
    } else {
        Ok(())
    }
}

fn parse_pipewire_target(output: &str) -> Result<Target, String> {
    let id = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("id "))
        .and_then(|line| line.split_once(", type PipeWire:Interface:Node"))
        .and_then(|(id, _)| id.parse::<u32>().ok())
        .ok_or("wpctl did not identify an output node")?;
    let serial = output
        .lines()
        .filter_map(|line| line.trim().trim_start_matches('*').trim().split_once('='))
        .find_map(|(key, value)| (key.trim() == "object.serial").then_some(value))
        .and_then(|value| value.trim().trim_matches('"').parse::<u64>().ok())
        .ok_or("wpctl did not identify the output node's lifetime")?;
    Ok(Target::PipeWire { id, serial })
}

fn parse_pipewire_mute(output: &str) -> Result<bool, String> {
    let mut parts = output
        .trim()
        .strip_prefix("Volume:")
        .ok_or("wpctl did not return a volume state")?
        .split_whitespace();
    parts
        .next()
        .and_then(|volume| volume.parse::<f64>().ok())
        .filter(|volume| volume.is_finite() && *volume >= 0.0)
        .ok_or("wpctl returned an invalid volume")?;
    match (parts.next(), parts.next()) {
        (None, None) => Ok(false),
        (Some("[MUTED]"), None) => Ok(true),
        _ => Err("wpctl returned an unknown mute state".into()),
    }
}

fn parse_pulse_mute(output: &str) -> Result<bool, String> {
    match output.trim() {
        "Mute: yes" => Ok(true),
        "Mute: no" => Ok(false),
        _ => Err("pactl returned an unknown mute state".into()),
    }
}

fn parse_alsa_card(output: &str) -> Result<String, String> {
    let card = output
        .lines()
        .find(|line| line.starts_with("Card "))
        .and_then(|line| line.split_once('\''))
        .and_then(|(_, rest)| rest.split_once('\''))
        .map(|(card, _)| card)
        .ok_or("amixer did not identify the default sound card")?;
    validate_identifier(card)?;
    Ok(card.into())
}

fn parse_alsa_mute(output: &str) -> Result<bool, String> {
    match (output.contains("[on]"), output.contains("[off]")) {
        (true, false) => Ok(false),
        (false, true) => Ok(true),
        // Do not flatten mixed channel state into one bool: unmuting all
        // channels later would overwrite a previously muted channel.
        _ => Err("ALSA Master mute is unknown or differs between channels".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipewire_requires_a_concrete_node_and_serial() {
        let output = "id 41, type PipeWire:Interface:Node\n  * object.serial = \"92\"\n";
        assert!(matches!(
            parse_pipewire_target(output),
            Ok(Target::PipeWire { id: 41, serial: 92 })
        ));
        assert!(parse_pipewire_target("id 41, type PipeWire:Interface:Node").is_err());
        assert!(parse_pipewire_target(
            "id 41, type PipeWire:Interface:Device\nobject.serial = \"92\""
        )
        .is_err());
    }

    #[test]
    fn unreadable_cli_state_is_not_unmuted() {
        assert!(parse_pipewire_mute("Volume: 0.35 [MUTED]\n").unwrap());
        assert!(!parse_pipewire_mute("Volume: 1.50\n").unwrap());
        assert!(parse_pipewire_mute("Audio is unavailable").is_err());
        assert!(parse_pipewire_mute("Volume: NaN").is_err());
        assert!(parse_pulse_mute("Mute: yes\n").unwrap());
        assert!(!parse_pulse_mute("Mute: no\n").unwrap());
        assert!(parse_pulse_mute("unknown").is_err());
        assert!(parse_alsa_mute("Front Left: [off]\nFront Right: [on]").is_err());
        assert!(parse_alsa_mute("No Master control").is_err());
    }

    #[test]
    fn alsa_resolution_extracts_the_card_id_not_default_alias() {
        let output = "Card default 'PCH'/'HDA Intel PCH at 0x1234'\n";
        assert_eq!(parse_alsa_card(output).unwrap(), "PCH");
        assert!(parse_alsa_card("Card default ''/'Unknown'").is_err());
        assert!(validate_identifier("@DEFAULT_SINK@").is_err());
    }
}

#[cfg(test)]
#[path = "output_mute_linux_test.rs"]
mod native_tests;
