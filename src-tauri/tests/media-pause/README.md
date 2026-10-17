# Media pause validation

The small test crate compiles the **production** controller and its Windows SMTC,
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
  user override, replacement owner, and delayed-reply regression. Excludes
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

## Controller behavior

Only sessions playing at recording readiness are paused. Stop, cancellation,
disabling the setting, and superseding recordings retire the same session's
lease. A successful method reply is insufficient: the controller also requires
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
Linux read/setup deadlines are per call, not a hard whole-batch time limit.

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
