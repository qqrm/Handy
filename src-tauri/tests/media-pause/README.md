# Media pause validation

This test crate compiles the actual production controller and Windows SMTC
adapter without the unrelated speech-model and Vulkan build. The Windows CI
also compiles all application targets with their normal native dependencies.

Run validation remotely through the **Windows media pause** GitHub Actions
workflow on the reviewed PR commit. Its `windows-native` job runs the lifecycle,
identity, cancellation, overlap and error tests, then compiles all application
targets. Its separate `windows-isolated-smtc` job runs both real SMTC checks
sequentially on a fresh GitHub-hosted `windows-2025` VM. PR updates trigger the
workflow; inspect both jobs under the PR's Checks tab.

**Never run real SMTC checks on a working Windows desktop**, even with a silent
fixture. Do not create a local VM or spoof the guard variables to bypass this
restriction. Both checks fail before any WinRT, session enumeration, AUMID or
MediaPlayer work unless the GitHub-hosted Windows environment and the job's
explicit `HANDY_RUN_ISOLATED_SMTC_TESTS` opt-in are present. They remain ignored
in the default test suite. The environment check prevents accidental launches;
it is not a sandbox. Isolation comes from the remote GitHub-hosted VM.

The second check creates a silent MediaPlayer fixture and passes only that
fixture's exact session to the production controller. It verifies pause/resume
and an observed user Play→Pause override, then closes the fixture. It never
controls other sessions. A hosted VM unable to expose SMTC or publish the
fixture fails the job. Treat that as a runtime coverage gap; do not skip it or
replace it with mock results to report a passing native check.

Only sessions playing at recording readiness are paused. Existing paused
sessions and replacement sessions from the same app are never resumed.
After an observed playback transition away from our acknowledged pause, the
session is left under user control, even if it becomes paused again.

SMTC exposes playback state, not the author of a command. A repeated Pause
while already Paused may emit no transition; the application cannot distinguish
that intent from its own pause. Unavailable sessions and failed commands are
best effort and do not prevent recording or affect other sessions.
