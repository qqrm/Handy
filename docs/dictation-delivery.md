# Dictation destination, clipboard completion, and recording reminders

These settings belong to the owner-requested dictation workflow in this fork.
They build on the recording cleanup changes in PR #12.

## Finish a recording

The regular transcription shortcut still starts recording and finishes it using
the selected activation mode. The recording-only **Finish to clipboard** shortcut
defaults to `Ctrl+Escape` on Windows and `Command+Escape` on macOS. It finishes the
current recording, transcribes it using the original action's post-processing
choice, and copies the complete result without inserting it or submitting it.

This is an alternative Stop command while recording. It neither starts a new
recording nor changes the destination after a regular Stop has already begun
recognition. Plain Escape continues to cancel. Recording-only registrations and
their queued callbacks are tied to the current recording generation, so an old
callback cannot end the next recording. Outside recording the shortcuts are
released back to the operating system.

Linux retains the application's existing restriction on dynamic recording-only
shortcuts; the new shortcut is hidden there.

## Choose the destination on Windows

**At recording start** is the default. **At recording end** supports starting a
dictation anywhere, placing the caret in the desired field, and then stopping.
Both choices pin the native field/caret anchor at the selected keyboard edge and
acquire the corresponding accessibility field and selection. The target is never
recaptured when recognition finishes. Changes to output settings apply to the
next recording.

The keyboard edge pins the native focus/caret anchor immediately and starts
accessibility acquisition in a worker. Slow providers do not delay microphone
stop or block the application UI. Acquisition has its own deadline; a completed
snapshot remains valid across long recognition, while a changed anchor or failed
acquisition falls back to the clipboard.

Delivery uses the following order:

1. For supported native Edit/RichEdit controls and non-Chromium UI Automation
   ValuePattern fields, insert through the captured target without activating
   another window.
2. Some browser editors provide text/selection access but no background writer.
   Their normal paste command is permitted only if the original field, document
   value, focus and caret/selection still match. The clipboard is checked again
   immediately before the key batch, and the original field is read back before
   treating the insertion as successful.
3. If the target is unavailable, changed, read-only, protected, or unsupported,
   keep the complete transcription in the ordinary clipboard for manual paste
   or Windows clipboard history (`Win+V`). The failure notification is optional.

Chromium/Edge editors always use the guarded foreground route even when they
advertise ValuePattern: native Windows testing showed that their SetValue action
can steal focus. After switching away from such an editor, the result therefore
stays in the clipboard; Handy does not attempt that background write.

Snapshots are one-shot. Process/window identity, accessibility identity, document
context, text and selection are checked where the provider exposes them. Closed
or recycled controls are rejected. Password/read-only fields are not captured as
editable destinations. Text used for these checks stays in memory and is not
included in notification payloads or diagnostic logs.

An uncertain write is never retried automatically. Its notification asks the user
to check the original field before manually pasting the retained text. A failed
clipboard write is reported as a clipboard error, not as successful copying.
Clipboard restoration happens only after verified foreground insertion while
the paste still owns the clipboard; a failure has no delayed restoration task
that could erase its fallback text.

Auto-submit is attempted only after verified insertion and another check of the
original focused field. Background insertion does not automatically submit into
another foreground application. Held physical modifiers and incomplete keyboard
batches suppress submission; accepted key-down prefixes are released without
replaying the command. When enabled, failure notifications also explain skipped
auto-submit.

Windows does not expose an atomic operation that compares accessibility focus
and clipboard ownership and then delivers `SendInput` to that exact control.
The foreground-only paste and auto-submit paths therefore retain a small
programmatic-focus/clipboard race between their last check and key injection.
Readback prevents an unconfirmed paste from authorizing auto-submit, but cannot
undo text already consumed by an application. Accessibility providers also vary
in which editable fields and document changes they expose. Background delivery
is not promised for every application.

Accessibility capture itself is not atomic with the keyboard edge. Switching
between virtual fields sharing the same native window before the worker has
installed its focus hooks can be invisible when their native caret anchors are
absent or identical. The current guard rejects observed focus/selection changes;
it does not promise to detect every such provider-level transition. Cancellation
also cannot undo an OS/provider write that has already begun.

macOS and Linux keep their existing regular insertion behavior in this change.

## Remind about a recording left running

**Remind while recording** is disabled by default. With it enabled, the first
notification defaults to one minute (configurable from 1 to 120 minutes). Each
following interval doubles: with a one-minute first interval, notifications appear
after 1, 3, 7, 15, ... minutes from recording start.

Reminders apply to toggle recordings and Hold-or-toggle sessions classified as
a tap. Holding a push-to-talk shortcut does not arm them. The elapsed clock is
independent of VAD, pauses, microphone activity and background speech.

**Add sound to notifications** adds the same short signal to each reminder,
including the first. There is no separate sound timer, volume escalation, forced
Stop, or change to insertion policy. Stop and Cancel invalidate queued reminders
and stop an active reminder sound. The popup does not take keyboard focus.

The bundled CC0 triple knock is `src-tauri/resources/reminder.wav`, sped up 2.1x
from its source (about 0.66 seconds). Its source and license are recorded in
`reminder.LICENSE.txt`. To replace it, put `custom_reminder.wav` in Handy's app data
directory. Reminder audio uses the selected feedback output device and volume,
independently of start/stop feedback being enabled. If that output is muted during
recording, the popup still appears but the sound is inaudible; Handy does not
unmute other audio to play the reminder.

## Verification scope

- `scripts/ci/coordinator_regressions.py` executes production lifecycle methods,
  output-selection policy and recording-scope generations with explicit external
  service fakes. Compiled mutations verify that the tests detect wrong output
  mode, stale shortcuts/releases, missing reminder cleanup and earlier
  Stop/Cancel races. Recognition itself is a labeled fake boundary in this suite.
- `src-tauri/tests/dictation-output` imports the actual output module. Its portable
  tests cover clipboard completion, cancellation and stale output ownership.
  Windows desktop cases use real input controls with external settings/clipboard
  transports identified as fakes.
- `src-tauri/tests/dictation-target` imports the real Windows target adapter and
  tests UTF-16/selection logic, request timeouts, native controls and foreground
  readback. Desktop fixtures must be run explicitly on a Windows desktop.
- `recording_reminders_core.rs` tests deadline progression and cancellation with
  deterministic clocks. Notification frontend tests use the real UI and a mocked
  event transport; browser layout checks do not prove native focus behavior.
- The existing Windows CI job runs native fixtures and compiles/lints the full
  Tauri application. Its results are reported separately from local cross-checks.

Pause-based partial recognition for local Whisper remains a separate feature.
