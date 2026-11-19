# Recording pause and mute validation

The small test crate compiles the **production** recording-output controller,
endpoint-mute adapters, media-pause controller, and its Windows SMTC,
Linux MPRIS, or macOS QuickTime adapter, without speech-model/GPU SDKs. Full application builds
remain separate checks. All builds and runtime tests for this change run on
standard GitHub-hosted VMs in this public fork. Never run the native player
fixtures on a working desktop or spoof their hosted-environment guards.

## Coverage contract

- **Windows x64:** actual SMTC enumeration and video-only MediaPlayer; advancing
  clock and frames; pause, resume, observed user override; full application
  compilation and MSI/NSIS packaging. Excludes audible output, transcription,
  all third-party players, and the cause of historical Explorer hangs.
- **Linux x64:** production MPRIS against two real VLC processes on a private
  D-Bus; advancing/paused clocks, initially paused player, multiple players,
  user override, replacement owner, and delayed-reply regression. The production
  PulseAudio mute adapter also runs against two null sinks on a private server,
  including a changed default output. Excludes
  audible hardware (dummy sink), every desktop/browser, and transcription.
- **macOS Intel:** production controller and QuickTime adapter regressions and
  actual playback through public Apple Events on a native Intel VM. Application
  packaging and executable startup also run, with the x64 package built on ARM
  and launched under Rosetta. Excludes other players and transcription.
- **macOS Apple Silicon:** the same controller/adapter and real QuickTime fixture
  on native ARM, native packaging and executable startup. Same exclusions as
  Intel; building alone is not playback validation.

This list describes the required checks, **not their current pass status**.
Inspect the exact PR commit's Checks tab. A failed or unavailable native fixture
is a coverage gap; do not skip it or replace it with mocks and report success.
The opt-in setting is available on Windows, Linux and macOS, and remains off by
default. macOS support is explicitly limited to QuickTime Player local files. A passing
fixture does not extend that support to Music, Spotify, VLC, or browsers.

## Pause + mute regression evidence

SMTC exposes media sessions, not every browser tab or every audio source. Pause
and mute therefore remain independent options. Pause preserves the position of
controllable media; mute silences the selected output while other media may
continue advancing. Neither option claims to stop every browser tab.

The recording-output controller snapshots the two options at the start of a
capture. A stop request rejects late readiness callbacks, but keeps existing
pause/mute ownership through the capture tail and actual recorder stop. Stop and
Cancel share the same cleanup boundary, including when the microphone stays
open in always-on or lazy-close mode. Cleanup finishes before the manager
publishes Idle. Delayed work from an older recording cannot mutate a newer one.
The coordinator also waits for both the Stop pipeline and the entire Cancel
worker, including stream/model and UI cleanup calls, before accepting a queued
restart. Repeated Cancel shares one cleanup worker. Each Start reports whether
that call actually started capture, rather than sampling an older recording.

Mute ownership records the exact output selected before its state is read or
changed. Restoration never targets a newly selected default. An already-muted
or unreadable output is left alone. Failed restoration retains ownership for a
later cleanup attempt without blocking mute on a healthy new default. Returning
to an output still muted by a retained lease preserves that owner. Retries happen
only after capture stops or closes, never at the start of another capture. At most
32 mute leases are retained; reaching that bound refuses another mute instead of
discarding an existing restoration obligation.
Settings changes affect the next capture; they do not release the current
capture's protection early.

### Evidence layers

| Layer                 | What is checked                                                                                                                                                                                                                                         | Limit                                                                                                                                                         |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Historical RED        | Unchanged `AudioRecordingManager` method bodies from `bf42308f755749e61b1dbda5bef613d629842d9a` reproduce AlwaysOn/lazy Cancel, early resume into tail capture, stale mute, and default-output retargeting                                              | Recorder, settings, output endpoints, SMTC transport, and lazy timer are explicit fake collaborators; this is not the whole application                       |
| Current runtime GREEN | Actual `recording_output.rs` and `output_mute.rs` with deterministic collaborators: all four settings combinations, stop/cancel ordering, delayed readiness, overlapping generations, original mute state, failed restoration, and output identity      | Does not open a microphone or render physical audio                                                                                                           |
| Stop/Cancel runtime   | The complete production coordinator and its tests, unchanged manager lifecycle methods, actual `TranscribeAction::start`, `FinishGuard`, no-token Stop entry, and centralized Cancel functions; channel gates exercise capture and post-capture cleanup | External microphone, model, UI and output services are explicit fakes; successful-token transcription and deferred platform UI tasks are outside this fixture |
| Mutation sensitivity  | Each deliberately reintroduced production-code fault must compile, then fail one exact previously-passing regression test                                                                                                                               | A compile error, timeout, skipped test, or zero matches never counts as detecting the fault                                                                   |
| Application wiring    | Source contracts check actual Stop/Cancel capture closures, protected tail, coordinator completion, stop-cue order, and Windows endpoint reopening; full application compilation remains mandatory                                                      | Static integration checks are reported separately from runtime controller tests                                                                               |
| Native Linux output   | Actual PulseAudio commands on an isolated server mute one sink, switch the default, and restore only the original sink                                                                                                                                  | Null sinks exercise the audio service, not speakers or acoustic microphone pickup                                                                             |
| Native Windows        | Actual SMTC/video-only playback and compilation of the production COM endpoint adapter                                                                                                                                                                  | Hosted Windows has no guaranteed audio endpoint; neither audible mute nor switching two physical Windows outputs is claimed                                   |

Run the complete portable evidence procedure with:

```bash
python3 scripts/ci/recording_output_regressions.py --output /tmp/handy-recording-output-evidence
```

The pinned historical commit must be present locally (`git fetch --unshallow`
when using a shallow clone). The script never mutates the working tree: current
production sources are copied into a temporary snapshot for mutation checks.
It records commit IDs, source/method SHA-256 hashes, selected tests, successful
compilation, and exact assertion outcomes in `receipt.json` with the associated
logs and extracted historical source. The historical fixture includes two
passing sanity checks; its five intentional RED assertions are evidence of the
old faults, not failures of the corrected implementation.
The nested `coordinator/receipt.json` records 43 coordinator/runtime tests and
four compiled mutations for premature completion, false Start success, early
Cancel completion and duplicate cleanup workers. The output-controller layer
checks ten compiled mutations, including failed restoration on an old output
blocking mute of a new default and restoration during a later capture.

CI runs this procedure once on Ubuntu, alongside ordinary Linux/macOS/Windows
tests and full Windows application compilation. The small
`recording-output-regression-evidence` artifact retains receipts and logs for
14 days; no installer or audio recording is uploaded. Read the receipt's
`checked_commit`, `working_tree_dirty`, and `status` before using it as evidence.
The repository contains the procedure so the proof can be regenerated after
artifact expiry. A green historical-only receipt does **not** establish current
behavior: the full receipt must say `all_expected_results_verified`.

## Controller behavior

Only sessions playing at recording readiness are paused. Capture completion,
cancellation cleanup, and stream shutdown retire the same session's lease;
changing an option affects the next capture. A successful method reply is insufficient: the controller also requires
the playback transition. Late Pause and Play operations retain their original
object, observer, and pending result; a new recording cannot overlap them.

The one-second acknowledgement deadline reduces polling; it never assumes that
an already delivered request has been cancelled. An uncertain operation can
suppress new media pauses until the original session disappears or Handy exits.
No command is automatically retried. Queued work and retained sessions are
bounded at 32. More than 32 enumerated sessions refuses the whole pause batch.
Recording itself does not wait for media control. Exit restoration has a
three-second best-effort window; outstanding provider work can outlive exit.

Observed playback away from the acknowledged pause permanently relinquishes
automatic resume ownership, including a rapid observed Play then Pause. Media
APIs do not identify command authors: a repeated Pause while already paused can
emit no transition, and suppressed/coalesced events cannot be reconstructed.
Windows synchronous COM calls are not made cancellable by asynchronous polling.
Linux and macOS read/setup deadlines are per call, not a hard whole-batch limit.

## Native fixtures

The **Desktop media pause** workflow runs shared regression tests and clippy on
Windows, Ubuntu, Intel macOS, and ARM macOS. Native fixtures are explicitly
opted in, ignored by ordinary `cargo test`, and reject execution outside a
GitHub-hosted job. Guards prevent accidents; the remote VM provides isolation.

Windows creates a real 30-second 64x64 MediaComposition clip with no audio stream.
Frame-server mode needs no display surface or audio endpoint. The test requires
clock movement and VideoFrameAvailable events, then passes only its own exact
session to the production controller. The historical WAV fixture remains a
separate ignored check: [the audio diagnostic run](https://github.com/qqrm/Handy/actions/runs/38740862070/job/116441888081)
found no audio endpoints and failed with `MF_E_CANNOT_CREATE_SINK` before media
control. It is not included in the video-only job or reported as passed.

Linux installs the distribution VLC and uses `dbus-run-session` for each test.
VLC decodes generated local PCM into its dummy sink. Only fixture processes
are controlled; PID and unique D-Bus-owner identity are checked. A separate
protocol service tests delayed replies and signal ordering. The product adapter
uses zbus directly and requires neither VLC nor playerctl.

The macOS adapter uses public PID-addressed Apple Events, launch identity, and
the original QuickTime window ID with an unchanged file association. QuickTime
returns documents by name, which is not a stable playback identity; playback
commands instead address the document property of the original window. Closed
or replaced windows are rejected. The fixture measures actual clock progression
and tests the production adapter/controller, including multiple documents.
Only the exact fixture-to-QuickTime consent dialog may be accepted inside the
fresh VM; no TCC database changes or global media keys are used. Missing consent,
a missing receipt, a timeout, or a non-advancing clock fails the check.

### macOS automatic resume limits

QuickTime Player local-file documents are supported in this version. Enabling the
setting requests macOS Automation permission for an already running QuickTime;
open QuickTime first, then enable the setting. Denial leaves the setting off and
playback alone. There are no consent prompts during recording. The app contains
Apple's required
[usage description](https://developer.apple.com/documentation/bundleresources/information-property-list/nsappleeventsusagedescription)
and [Apple Events entitlement](https://developer.apple.com/documentation/BundleResources/Entitlements/com.apple.security.automation.apple-events).
It never launches a closed player or opens/reopens a media file.

Automatic resume is best effort: an observed manual playback change, lost
identity or read error revokes resume. Polling cannot detect a quick Play then
Pause between reads, a repeated Pause in the same state, or every same-file
replacement inside a surviving window. Process/window/file checks reduce
retargeting risk but are not an atomic media-session generation token. The
accepted policy preserves automatic resume with these limits; it does not
promise perfect detection of user intent. Apple Event timeouts do not prove
that a submitted command was cancelled, so uncertain operations are retained
without retries and can suppress later pauses until the original player exits.
The shared controller is the sole QuickTime state poller; its watch guard binds
ownership for command preflight rather than starting a competing observer.

The separate **Fork artifact packaging** workflow builds test packages without
release credentials, publication, or retained installers. Both macOS targets
are built on ARM to catch build-script host/target confusion. The audit checks
Mach-O architecture, ad-hoc signature, absence of updater signatures, and
bounded startup of the packaged executable: help within 30 seconds, first device
enumeration within 120 seconds for cold Metal shader compilation, and a repeat
within 30 seconds with identical device identities including CPU. A timeout retains
a process stack diagnostic and fails the job. The separate cold budget follows
[a measured 31.450-second Metal compile](https://github.com/qqrm/Handy/actions/runs/38931717338/job/117077161441),
which completed enumeration after the original 30-second gate had expired.
Ad-hoc packages are test outputs, not signed/notarized distribution releases.

## Upstream comparison

Reviewed upstream main `9e57a0d7929fcc6417ea0d6e54c1b57f62635edd`,
[PR #1028](https://github.com/cjpais/Handy/pull/1028) at
`2fa2a0d5c3ca406b7a2bea8d1394db8ac14b5c5b`, and
[PR #2054](https://github.com/cjpais/Handy/pull/2054) at
`7b034092dabb4a3f50eca094d21013a4533ce7aa`. Neither PR was merged when reviewed.
#1028 has author-reported manual coverage across three operating systems; that
is useful evidence, not a reason to assume this fork is universally better.

#1028's Windows/Linux paths act on the first playing session and restore by a
reusable app/service identifier. Its macOS path queries private MediaRemote
through a Perl-loaded library and sends a global Play/Pause toggle. This fork's
Windows/Linux paths retain concrete session identity, cover multiple players,
and preserve observed user overrides and late acknowledgements.

#2054 explores public Apple Events for named macOS applications and opt-in
browser JavaScript. Its Linux `dbus-send` fallback omits method-call mode, so a
successful tool exit does not establish that Pause/Play was invoked. The fork
uses direct typed D-Bus method calls. The macOS adapter supports only the checked
QuickTime transport; it does not claim parity with all of #2054's targets.

Public Apple Events alone do not make every player a suitable adapter. VLC's
[reviewed macOS AppleScript `play` handler](https://github.com/videolan/vlc/blob/3.0.x/modules/gui/macosx/applescript.m)
toggles playback; reading its state before a toggle does not make an explicit
Pause/Play operation race-free.
Music/Spotify track identifiers describe content, not a playback generation;
their actual playback and current application dictionaries remain unvalidated.
QuickTime is the only public macOS transport exercised by this change. Polling
cannot reconstruct Play/Pause transitions between reads, and
[distributed notifications](https://developer.apple.com/documentation/foundation/distributednotificationcenter)
can be delayed or dropped. The explicit QuickTime-only scope and documented
best-effort automatic resume are intentional; other players need their own
identity, explicit-command and native-playback validation before support expands.
