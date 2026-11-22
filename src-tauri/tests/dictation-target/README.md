# Pinned dictation target checks

This small harness compiles the actual production `dictation_target` module
without building audio models or Tauri. It does not substitute a fake delivery
implementation.

```sh
cargo test --manifest-path src-tauri/tests/dictation-target/Cargo.toml --locked
cargo clippy --manifest-path src-tauri/tests/dictation-target/Cargo.toml --all-targets -- -D warnings
```

On a Windows desktop with Microsoft Edge installed, run the explicit fixtures
as well. These interact with their own visible windows and must run serially,
with one scenario per process as in the Windows CI job:

```sh
cargo test --manifest-path src-tauri/tests/dictation-target/Cargo.toml --locked native_fixture::native_control_lifecycle_fixture -- --exact --ignored --test-threads=1
cargo test --manifest-path src-tauri/tests/dictation-target/Cargo.toml --locked native_fixture::foreground_readback_fixture -- --exact --ignored --test-threads=1
cargo test --manifest-path src-tauri/tests/dictation-target/Cargo.toml --locked native_fixture::deferred_capture_fixture -- --exact --ignored --test-threads=1
cargo test --manifest-path src-tauri/tests/dictation-target/Cargo.toml --locked edge_fixture::edge_contenteditable_delivery_fixture -- --exact --ignored --test-threads=1
```

The fixture creates two real Win32 edit controls and uses the public production
capture/delivery API. It checks background insertion at the captured selection,
unchanged second-field contents, one-shot delivery, focus/caret guards, document
change rejection, read-only/password rejection, and closed-control rejection.

The foreground fixture uses the real native Edit control's UIA TextPattern,
forcing only the capability choice with a `cfg(test)` helper. It covers a moved
focus/caret, asynchronous readback, cancellation and a mismatching result. It
does not pretend to exercise a particular browser's accessibility provider.

`deferred_capture_fixture` holds acquisition behind a channel gate and checks
that beginning capture returns without waiting for it. It then uses the real
provider and verifies that a completed capture survives a transcription longer
than the original four-second acquisition deadline.

`edge_fixture::edge_contenteditable_delivery_fixture` launches installed Edge
with a fresh temporary profile and a local page containing a contenteditable
field and an unrelated textarea. It uses the natural production capability
selection, never the forced foreground helper. For a foreground-only provider it
checks moved-focus rejection, publishes real Unicode clipboard text, issues one
Ctrl+V batch and verifies production readback. If the provider exposes the
addressed UIA route, it verifies background insertion and preservation of focus.
Both routes require the unrelated field to remain unchanged. Missing Edge,
unsupported capture, or a failed delivery fails the test. Only the fixture's
private process job and temporary profile are cleaned up. The fixture temporarily
uses the desktop clipboard; it restores prior Unicode text while it still owns
that clipboard version. Use the dedicated test desktop, not an active user
session with important non-text clipboard data.

Cross-compilation checks API compatibility but does not establish browser
support. The Edge route is validated only after this headed test passes on the
Windows runner; provider and application differences still require safe
clipboard fallback.

## Supported targets and limits

- `TargetCapture::begin` pins native foreground/focus and available native caret
  coordinates at the accepted shortcut edge, then acquires the UIA field and
  selection on its owning worker. It does not wait for provider calls on the
  audio-stop path. Focus events during acquisition and changes to the native
  anchor reject capture; a ready result can be resolved after long transcription.
  A virtual field switch within the same HWND before the worker installs its
  hooks may be unobservable if native caret coordinates are unavailable or
  unchanged. The asynchronous UIA acquisition is not an atomic hotkey-time
  snapshot of arbitrary application fields.
- Native Unicode Edit/RichEdit controls use addressed `EM_SETSEL` and
  `EM_REPLACESEL`, with complete value and selection checks. RichEdit CRLF
  offsets must agree with UI Automation before insertion is allowed.
- Other controls require both UIA ValuePattern and TextPattern. The complete
  text and a single unambiguous selection must agree between those patterns.
  ValuePattern updates that same element and the result is read back. Since a
  browser can acknowledge SetValue before its renderer applies it, confirmation
  waits up to 750 ms within the original request deadline while the old value
  remains intact. Only reads are retried; the write is issued once. A generic
  browser contenteditable or application editor can still be unsupported.
- Chromium/Edge providers (`FrameworkId = Chrome` or a `Chrome_*` native host)
  use the foreground-only route even when ValuePattern is exposed. The real
  Edge fixture demonstrated that SetValue can move focus to its target as a
  provider side effect. If that field is no longer focused, delivery falls back
  to the clipboard without attempting SetValue or restoring focus afterwards.
- A focused Edit/Document with TextPattern but no ValuePattern can retain a
  foreground-only target. This requires a keyboard-focusable field with an
  explicit `IsReadOnly = false` text attribute; mixed/unknown attributes are
  rejected. The module arms and checks the original value/selection, and the
  caller may then issue one guarded paste. Full value/selection reads precede
  the final focus check. Changed focus or selection requires clipboard fallback.
  Confirmation retries only readback of that one paste for up to 750 ms while
  the original value remains unchanged, never the input itself.
- Process lifetime, native window identity, window destruction, UIA runtime
  identity, ancestor names, window title and original value are checked. These
  checks reject observable document changes; they cannot infer an application's
  invisible semantic switch that reuses exactly the same empty input element.
- The OS/provider exposes no generic compare-and-swap text edit. Validation and
  a native/UIA write are separate operations, so a concurrent application edit
  can race them. A missing or mismatching readback is **uncertain**, never an
  automatic retry or permission to paste into another field.
- A caller timeout atomically aborts an unclaimed write. A worker paused during
  preflight cannot later start that write after a definite timeout fallback.
  If the worker already claimed the write, a timeout is reported as uncertain.
- This module never changes foreground focus, sends global keyboard input, or
  touches the clipboard. A later auto-submit focus check is a best-effort check;
  it cannot make another module's global SendInput atomic with foreground focus.
  Background delivery never arms auto-submit, and does not invoke UIA Select
  to restore its caret.
- Captured field contents are held in memory for this recording only and never
  logged. Password and read-only fields are rejected before reading their value.

API references:

- https://learn.microsoft.com/en-us/windows/win32/controls/em-replacesel
- https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendmessagetimeoutw
- https://learn.microsoft.com/en-us/windows/win32/api/uiautomationclient/nf-uiautomationclient-iuiautomationvaluepattern-setvalue
