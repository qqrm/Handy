# Dictation output integration fixture

This crate imports the production `dictation_output/mod.rs`, its `policy.rs`,
and `dictation_target` directly. It does not copy or rewrite their implementation.

External fakes provide Tauri state borrowing, persisted settings, clipboard
writes, notification delivery and the non-Windows legacy paste transport.
The Windows target and `SendInput` glue compile against the real Windows APIs.
The ignored Windows tests create real Edit controls using the target fixture;
they exercise captured Start/End destinations, frozen settings, closed controls
and clipboard-only completion. They never intentionally submit global keys.
The positive delivery case keeps focus stable until the pending acquisition is
resolved and requires an actual insertion. Focus changes during pending
acquisition may instead retain the complete text in clipboard; race cases
require the later focus to remain untouched in either valid outcome.
`PreparedOutput: Send` is compile-checked for the blocking delivery worker.

```sh
cargo test --manifest-path src-tauri/tests/dictation-output/Cargo.toml
cargo clippy --manifest-path src-tauri/tests/dictation-output/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path src-tauri/tests/dictation-output/Cargo.toml --all-targets --target x86_64-pc-windows-msvc -- -D warnings
# On an interactive Windows test desktop only, one scenario per process:
cargo test --manifest-path src-tauri/tests/dictation-output/Cargo.toml windows_output::output_resolves_pending_capture_and_uses_frozen_settings -- --exact --ignored --test-threads=1
```

The Windows CI enumerates all five output scenarios as separate exact commands.
This preserves foreground activation eligibility when the previous scenario has
destroyed its last GUI thread; each command must execute one passing test.

Linux execution verifies clipboard completion, cancellation, stale-session
rejection, fallback/error reporting and the portable capture-moment policy.
Windows cross-checking verifies compilation; it is not Windows runtime evidence.
The clipboard fake does not establish actual Windows clipboard history behavior.
