# Media pause validation

This test crate compiles the actual production controller and Windows SMTC
adapter without the unrelated speech-model and Vulkan build. The Windows CI
also compiles all application targets with their normal native dependencies.

Run the lifecycle, identity, cancellation, overlap and error tests:

```powershell
cargo test --locked --manifest-path src-tauri/tests/media-pause/Cargo.toml
```

On an interactive Windows desktop, explicitly opt into the native checks:

```powershell
cargo test --locked --manifest-path src-tauri/tests/media-pause/Cargo.toml windows_enumeration_is_read_only -- --ignored --nocapture
cargo test --locked --manifest-path src-tauri/tests/media-pause/Cargo.toml isolated_windows_session -- --ignored --nocapture --test-threads=1
```

The second check creates a silent MediaPlayer fixture and passes only that
fixture's exact session to the production controller. It verifies pause/resume
and an observed user Play→Pause override, then closes the fixture. It never
controls other sessions. A desktop unable to publish the fixture fails this
opt-in test; that is a runtime coverage limit, not a passing hardware check.

Only sessions playing at recording readiness are paused. Existing paused
sessions and replacement sessions from the same app are never resumed.
After an observed playback transition away from our acknowledged pause, the
session is left under user control, even if it becomes paused again.

SMTC exposes playback state, not the author of a command. A repeated Pause
while already Paused may emit no transition; the application cannot distinguish
that intent from its own pause. Unavailable sessions and failed commands are
best effort and do not prevent recording or affect other sessions.
