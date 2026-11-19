#!/usr/bin/env python3
"""Evidence for the legacy-branch backport; no audio devices are accessed.

Current tests and mutants compile production modules from the focused harness.
Historical RED compiles the unchanged Windows policy from the pinned old head,
with two new assertions appended to its existing fake-session tests. Compile
failures, timeouts and zero matching tests never count as detected regressions.
Static call-site guards complement these tests; they are not application builds.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[2]
BASELINE = "e6f12faac5c93b6a2ab967bac96b3ead10cec1ec"
MANIFEST = Path("src-tauri/tests/media-control/Cargo.toml")
CONTROLLER = "src-tauri/src/managers/recording_output.rs"
WORKER = "src-tauri/src/media_control_worker.rs"
POLICY = "src-tauri/src/media_control_windows_batch.rs"

MUTANTS = (
    ("missing_capture_restore", CONTROLLER,
     "        state.restore_mute();\n        self.0.backend.resume();\n        state.recording = None;",
     "        self.0.backend.resume();\n        state.recording = None;",
     "recording_output::tests::cancel_restores_output_with_open_microphone"),
    ("restore_before_capture_tail", CONTROLLER,
     "        let result = capture();\n        drop(finish);",
     "        drop(finish);\n        let result = capture();",
     "recording_output::tests::stop_keeps_output_protected_through_tail"),
    ("accept_stopped_readiness", CONTROLLER,
     "if recording.generation != generation || recording.phase != Phase::AwaitingReady {",
     "if recording.generation != generation {",
     "recording_output::tests::stale_readiness_cannot_mute_after_finish"),
    ("stale_finish_stops_new_capture", CONTROLLER,
     "if recording.generation != generation || recording.phase == Phase::Finishing {",
     "if recording.phase == Phase::Finishing {",
     "recording_output::tests::stale_finish_does_not_restore_new_recording"),
    ("pause_disables_mute", CONTROLLER,
     "if options.mute && state.muted.len() < MAX_MUTE_LEASES {",
     "if options.mute && !options.pause && state.muted.len() < MAX_MUTE_LEASES {",
     "recording_output::tests::all_four_setting_combinations_are_independent"),
    ("failed_restore_disables_new_output_mute", CONTROLLER,
     "if options.mute && state.muted.len() < MAX_MUTE_LEASES {",
     "if options.mute && state.muted.is_empty() {",
     "recording_output::tests::failed_restore_does_not_disable_new_default_mute"),
    ("retry_old_restore_inside_new_capture", CONTROLLER,
     "        self.0.backend.begin(generation, options.resume);",
     "        state.restore_mute();\n        self.0.backend.begin(generation, options.resume);",
     "recording_output::tests::returning_to_retained_output_never_restores_into_a_new_capture"),
    ("discard_legacy_resume_setting", CONTROLLER,
     "        self.0.backend.begin(generation, options.resume);",
     "        self.0.backend.begin(generation, true);",
     "recording_output::tests::legacy_resume_policy_is_captured_for_each_recording"),
    ("restore_changed_default", "src-tauri/src/managers/output_mute.rs",
     "self.backend.set_muted(&self.target, false)?;",
     "self.backend.set_muted(&self.backend.default_target()?, false)?;",
     "output_mute::tests::restore_uses_original_output_after_default_changes"),
    ("late_pause_forces_resume", WORKER,
     "resume: request.resume,", "resume: true,",
     "media_control::worker::tests::late_success_keeps_the_original_no_resume_policy"),
    ("stale_apply_reaches_new_generation", WORKER,
     "if request.generation != generation || request.ready {",
     "if request.ready {",
     "media_control::worker::tests::stale_apply_cannot_pause_a_new_recording"),
    ("processing_releases_pending_cancellation", "src-tauri/src/coordinator_cleanup.rs",
     "        !self.cancellation_pending", "        true",
     "coordinator_cleanup::tests::stop_completion_waits_for_full_cancellation_cleanup"),
)


def run(command: list[str], cwd: Path, log: Path, timeout: int = 120,
        env: dict[str, str] | None = None) -> tuple[int, str]:
    try:
        result = subprocess.run(command, cwd=cwd, env=env, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                timeout=timeout, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        log.write_text(str(error) + "\n")
        raise RuntimeError(f"Infrastructure failure: {command[0]}: {error}") from error
    log.write_text(result.stdout)
    return result.returncode, result.stdout


def expected_red(code: int, output: str, test: str, marker: str | None = None) -> None:
    assert code == 101, f"Expected a Rust assertion failure for {test}, got {code}"
    assert f"test {test} ..." in output, f"The named test did not execute: {test}"
    assert re.search(r"test result: FAILED\. 0 passed; 1 failed; 0 ignored;", output), test
    assert "panicked at" in output, f"No runtime assertion panic for {test}"
    assert "error[E" not in output, f"Compilation failure is not regression evidence: {test}"
    if marker:
        assert marker in output, f"Wrong historical assertion failed: {test}"


def expected_green(code: int, output: str, test: str) -> None:
    assert code == 0, f"Baseline failed for {test}: exit {code}"
    assert f"test {test} ..." in output, f"The named baseline did not execute: {test}"
    assert re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored;", output), test


def function(source: str, name: str) -> str:
    match = re.search(r"\bfn\s+" + re.escape(name) + r"\s*\(", source)
    assert match, name
    start = source.index("{", match.end())
    depth = 1
    end = start + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end]


def wiring() -> list[str]:
    read = lambda path: (ROOT / "src-tauri/src" / path).read_text()
    audio, actions = read("managers/audio.rs"), read("actions.rs")
    stop, cancel = function(audio, "stop_recording"), function(audio, "cancel_recording")
    checks = {
        "stop_capture_and_tail_inside_output_owner": stop.index("finish_after_capture") < stop.index("extra_recording_buffer_ms") < stop.index("rec.stop()"),
        "cancel_capture_before_manager_idle": cancel.index("finish_after_capture") < cancel.index("rec.stop()") < cancel.index("finish_recording_state"),
        "no_legacy_unscoped_mute": "fn apply_mute(" not in audio and "remove_mute()" not in actions,
        "cleanup_finishes_before_idle": function(audio, "finish_recording_state").index("stop_microphone_stream") < function(audio, "finish_recording_state").index("RecordingState::Idle"),
        "settings_include_legacy_resume_snapshot": "resume: settings.play_after_recording" in function(audio, "try_start_recording"),
        "mode_and_device_changes_hold_recording_state": all(function(audio, name).index("self.state.lock()") < function(audio, name).index("stop_microphone_stream") for name in ("update_mode", "update_selected_device")),
    }
    transcribe_stop = actions[actions.index("    fn stop(", actions.index("impl ShortcutAction for TranscribeAction")):]
    checks["guard_and_stop_token_before_spawn"] = transcribe_stop.index("FinishGuard") < transcribe_stop.index("request_recording_stop") < transcribe_stop.index("async_runtime::spawn")
    checks["stop_sound_after_capture"] = transcribe_stop.index("rm.stop_recording(&binding_id, generation, cancel_generation)") < transcribe_stop.index("SoundType::Stop")
    utils = read("utils.rs")
    checks["cancel_frontdoor_only_queues_cleanup"] = "coordinator.cancel()" in function(utils, "cancel_current_operation") and "audio_manager.cancel_recording()" in function(utils, "finish_cancellation")
    coordinator = re.sub(r"\s+", " ", read("transcription_coordinator.rs"))
    checks["coordinator_waits_for_both_completions"] = all(f"if cleanup.{name}() {{ stage = Stage::Idle; }}" in coordinator for name in ("finish_processing", "finish_cancellation"))
    checks["coordinator_coalesces_cancel_workers"] = "if cleanup.start_cancellation() {" in coordinator and "let _guard = CancellationGuard(app.clone()); crate::utils::finish_cancellation(&app);" in coordinator
    checks["coordinator_uses_own_start_result"] = "if action.start(app, binding_id, hotkey_string) {" in coordinator
    resume = function(read("media_control_windows.rs"), "resume_sessions")
    checks["windows_restore_uses_owned_handles"] = "request_manager" not in resume and "enumerate_sessions" not in resume and "windows_batch::resume_sessions(&paused)" in resume
    recorder = read("audio_toolkit/audio/recorder.rs")
    checks["closed_recorder_returns_error_without_wait"] = all('Error::other("Recorder is not open")' in function(recorder, name) for name in ("start", "stop"))
    assert all(checks.values()), f"Production wiring failed: {[name for name, ok in checks.items() if not ok]}"
    return list(checks)


def historical(rustc: str, evidence: Path) -> dict:
    source = subprocess.check_output(["git", "show", f"{BASELINE}:{POLICY}"], cwd=ROOT, text=True)
    additions = '''
    #[test]
    fn regression_prepaused_same_app_sibling() {
        let sessions = [TestSession::playing("same-app"), TestSession {
            status: Cell::new(Some(PlaybackStatus::Paused)),
            ..TestSession::playing("same-app")
        }];
        let paused = pause_sessions(&sessions);
        resume_sessions(&sessions, &paused).unwrap();
        assert_eq!(sessions[1].calls_to("play"), 0,
            "REGRESSION: prepaused sibling resumed by shared app ID");
    }
    #[test]
    fn regression_replacement_same_app_session() {
        let original = TestSession::playing("same-app");
        let paused = pause_sessions(std::slice::from_ref(&original));
        let replacement = TestSession {
            status: Cell::new(Some(PlaybackStatus::Paused)),
            ..TestSession::playing("same-app")
        };
        resume_sessions(std::slice::from_ref(&replacement), &paused).unwrap();
        assert_eq!(replacement.calls_to("play"), 0,
            "REGRESSION: replacement resumed by old app ID");
    }
'''
    cases = (
        ("regression_prepaused_same_app_sibling", "REGRESSION: prepaused sibling resumed by shared app ID"),
        ("regression_replacement_same_app_session", "REGRESSION: replacement resumed by old app ID"),
    )
    with tempfile.TemporaryDirectory(prefix="handy-legacy-red-") as directory:
        temp = Path(directory)
        last = source.rindex("\n}")
        (temp / "policy.rs").write_text(source[:last] + additions + source[last:])
        (temp / "probe.rs").write_text('''extern crate self as log;
#[macro_export] macro_rules! debug { ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }}; }
#[path = "policy.rs"] mod windows_batch;
''')
        binary = temp / "probe"
        code, _ = run([rustc, "--edition=2021", "--test", str(temp / "probe.rs"), "-o", str(binary)], temp, evidence / "historical-compile.log")
        assert code == 0, "Historical source did not compile; this is not RED evidence"
        for name, marker in cases:
            test = "windows_batch::tests::" + name
            code, output = run([str(binary), test, "--exact", "--nocapture"], temp, evidence / f"historical-{name}.log")
            expected_red(code, output, test, marker)
    return {"revision": BASELINE, "path": POLICY, "source_sha256": hashlib.sha256(source.encode()).hexdigest(), "runtime_failures": [case[0] for case in cases]}


def source_files() -> list[Path]:
    paths = list((ROOT / "src-tauri/src").glob("media_control*.rs"))
    paths += list((ROOT / "src-tauri/src/managers").glob("output_mute*.rs"))
    paths += list((ROOT / "src-tauri/src/managers").glob("recording_output*.rs"))
    paths += [ROOT / "src-tauri/src/coordinator_cleanup.rs"]
    paths += [ROOT / MANIFEST, ROOT / MANIFEST.with_name("Cargo.lock"), ROOT / MANIFEST.with_name("lib.rs")]
    return sorted(paths)


def source_hashes() -> dict[str, str]:
    paths = source_files()
    paths += [ROOT / path for path in (
        "src-tauri/src/actions.rs", "src-tauri/src/managers/audio.rs",
        "src-tauri/src/audio_toolkit/audio/recorder.rs", "src-tauri/src/lib.rs",
        "src-tauri/src/managers/mod.rs", "src-tauri/src/transcription_coordinator.rs",
        "src-tauri/src/utils.rs", "src-tauri/Cargo.toml", "src-tauri/Cargo.lock",
        "scripts/ci/media_control_regressions.py",
    )]
    return {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(paths)}


def git_metadata() -> dict:
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    dirty = subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)
    return {"checked_commit": commit, "working_tree_dirty": bool(dirty.strip())}


def mutations(cargo: str, evidence: Path, receipt: dict) -> None:
    receipt["isolated_baselines"] = []
    receipt["mutants"] = []
    with tempfile.TemporaryDirectory(prefix="handy-legacy-mutants-") as directory:
        snapshot = Path(directory)
        for source in source_files():
            target = snapshot / source.relative_to(ROOT)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
        target = ROOT / "target/media-control-regressions"
        command = [cargo, "test", "--locked", "--offline", "--manifest-path", str(MANIFEST),
                   "--target-dir", str(target)]

        def clean_and_compile(label: str) -> dict:
            clean, _ = run([cargo, "clean", "--manifest-path", str(MANIFEST),
                            "--target-dir", str(target), "--package", "handy-media-control-tests"],
                           snapshot, evidence / f"{label}-clean.log")
            assert clean == 0, f"Could not isolate harness artifacts for {label}"
            compiled, _ = run(command + ["--no-run"], snapshot,
                              evidence / f"{label}-compile.log", timeout=180)
            assert compiled == 0, f"{label} did not compile; this is not regression evidence"
            return {"clean_exit": clean, "compile_exit": compiled}

        receipt["isolated_baseline_compile"] = clean_and_compile("isolated-baseline")
        for test in dict.fromkeys(mutant[4] for mutant in MUTANTS):
            code, output = run(command + [test, "--", "--exact", "--nocapture", "--test-threads=1"],
                               snapshot, evidence / f"green-{test.replace('::', '-')}.log")
            expected_green(code, output, test)
            receipt["isolated_baselines"].append({"test": test, "exit_code": code})

        for name, file, before, after, test in MUTANTS:
            source = snapshot / file
            original = source.read_text()
            assert original.count(before) == 1, f"Mutation anchor is not unique: {name}"
            try:
                source.write_text(original.replace(before, after, 1))
                result = clean_and_compile(f"mutant-{name}")
                code, output = run(command + [test, "--", "--exact", "--nocapture", "--test-threads=1"],
                                   snapshot, evidence / f"mutant-{name}-runtime.log")
                expected_red(code, output, test)
                receipt["mutants"].append({"name": name, "test": test, **result,
                                           "runtime_exit": code,
                                           "result": "runtime assertion failed as expected"})
            finally:
                source.write_text(original)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    cargo = shutil.which("cargo") or "/home/agent/.cargo/bin/cargo"
    rustc = shutil.which("rustc") or "/home/agent/.cargo/bin/rustc"
    receipt = {"status": "incomplete"}
    try:
        receipt.update(git_metadata())
        receipt["source_sha256"] = source_hashes()
        receipt["wiring_guards"] = wiring()
        receipt["historical"] = historical(rustc, evidence)
        command = [cargo, "test", "--locked", "--offline", "--manifest-path", str(MANIFEST)]
        code, output = run(command, ROOT, evidence / "current-green.log", timeout=180)
        assert code == 0 and re.search(r"test result: ok\. [1-9][0-9]* passed; 0 failed;", output), "Current GREEN suite must pass before mutations"
        receipt["current_green"] = re.search(r"test result: ok\.[^\n]+", output).group(0)
        mutations(cargo, evidence, receipt)
        assert receipt["source_sha256"] == source_hashes(), "Production inputs changed during the proof run"
        receipt["status"] = "passed"
    except BaseException as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        for key, collect in (("ending_git_state", git_metadata),
                             ("source_sha256_after", source_hashes)):
            try:
                receipt[key] = collect()
            except Exception as error:
                receipt[key] = {"error": str(error)}
                receipt["status"] = "incomplete"
        receipt["scope"] = "Production modules and static integration guards; native desktop and full application builds are separate CI gates."
        (evidence / "summary.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"status": receipt["status"], "historical_red": 2,
                      "current_green": receipt["current_green"],
                      "isolated_green_baselines": len(receipt["isolated_baselines"]),
                      "runtime_mutants_detected": len(receipt["mutants"]),
                      "wiring_guards": len(receipt["wiring_guards"]),
                      "evidence": str(evidence)}, indent=2))


if __name__ == "__main__":
    main()
