# Recording output regression checks

This harness compiles the production media worker, Windows session policy,
recording output controller, output mute adapters, and coordinator cleanup gate
without Tauri or inference dependencies. The application remains based on the
original `win-pause-all-sessions` branch; its recorder, VAD, models, and Linux/macOS
media providers are retained.

## Behavior covered

- Pause and mute are independent. All four combinations are tested.
- Stop rejects late output activation immediately, but keeps existing protection
  through the configured capture tail and the actual recorder stop.
- Cancel restores output after capture stops, including always-on and lazy-close
  microphone modes. A stale finish cannot stop or restore a newer recording.
- Each recording snapshots all three existing settings: `mute_while_recording`,
  `pause_while_recording`, and `play_after_recording`. Changes apply to the next
  recording. In particular, a recording begun with resume disabled must never
  resume its playback, even when its Pause operation finishes after cancellation
  or the setting is enabled meanwhile.
- System media commands run on a separate worker, outside the request and output
  mutexes. Tests block a provider call and verify that capture cleanup requests
  still complete and retain their original resume policy.
- Windows owns each concrete session that accepted Pause. It never finds a
  replacement by app ID at resume time. A prepaused sibling or replacement with
  the same `SourceAppUserModelId` cannot inherit ownership.
- Mute restoration targets the captured output identity. Already-muted and
  unreadable outputs are not claimed. Failed restorations remain available for
  later cleanup, while a healthy new default can still be muted. Retained leases
  are bounded, and beginning another recording never restores audio into it.
- Stop processing and full cancellation cleanup must both finish before the
  coordinator accepts another recording. Repeated Cancel requests share one
  cleanup worker; unexpected completion events do not release a new recording.

## Portable checks

```sh
cargo test --locked --manifest-path src-tauri/tests/media-control/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/tests/media-control/Cargo.toml --all-targets -- -D warnings
```

Linux compiles the unchanged zbus 4.4 provider. Windows also compiles the actual
WinRT/Core Audio adapters and the native fixture. The isolated desktop tests are
ignored by these ordinary commands.

The additional proof runner records individual logs and a JSON receipt:

```sh
python scripts/ci/media_control_regressions.py --evidence /tmp/handy-media-evidence
```

It requires the old head `e6f12faac5c93b6a2ab967bac96b3ead10cec1ec` to be present
in Git. CI fetches that exact object. Two historical assertions reproduce the
prepaused-sibling and replacement-session bugs against the unchanged old Windows
policy. The current suite then passes, and targeted mutations of the actual
production modules must fail their named runtime assertions. A compilation error,
timeout, or zero matching tests cannot count as a detected regression. Static
call-site checks cover the integration with recording, cancellation, settings,
and close/exit paths; they complement the full application build.

Mutations use an isolated source snapshot and a separate target directory. Every
selected test first passes unchanged; each mutant then cleans only the harness
package, compiles successfully, and fails its exact runtime assertion. Dependency
artifacts remain cached. The receipt records the checked commit, dirty-worktree
status, and source hashes, and is written as incomplete if any step fails.

## Native CI evidence

The existing Windows job executes
`media_control::platform_windows::native_test::isolated_windows_video_pause_resume_and_legacy_policy`
exactly once on a GitHub-hosted VM. It creates its own silent video player, filters
enumeration to that fixture, and passes only its session to production policy and
worker code. It checks advancing playback clock/frames, actual Pause, actual
resume, initially paused playback, and disabled auto-resume. A video-only source
avoids relying on an audio render device on the VM.

The Linux job's native mute test starts its own PulseAudio daemon with two null
sinks. It verifies restoration of the original sink after the default changes,
including an already-muted replacement. It cannot access the developer's sound
server, PipeWire, ALSA, or physical devices. Both native fixtures require their
explicit opt-in and GitHub-hosted environment checks; do not spoof these checks
on a working desktop.

The Windows application job separately compiles and links the real debug app,
including module registration and the old recorder/coordinator integration.
Focused harness results alone do not establish that the complete app builds.

For final publication, the existing `win-pause-all-sessions` branch may reuse a
successful validation-branch run only when it has the **same commit SHA** and all
three required jobs of the **same workflow** passed. This avoids duplicating an
identical build after the external PR is updated. Pull-request and manual runs,
other branches, and commits without matching successful evidence run the checks
normally.

## Scope limits

SMTC exposes supported sessions, not every sounding browser tab. Mute protects
the output captured for that recording; changing the system default during the
same recording does not automatically mute the new output. Neither a video-only
SMTC fixture nor null sinks prove audible playback on physical hardware.

The old platform pause policies remain otherwise unchanged. Windows checks the
owned session's status before resuming, but this backport does not subscribe to
manual playback changes: user Play followed by user Pause on the same owned
session cannot be distinguished from Handy's pause at recording end. The native
fixture does not claim to cover that override sequence. Linux retains its chosen
MPRIS-player behavior, and macOS retains its existing MediaRemote behavior.
