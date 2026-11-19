#!/usr/bin/env python3
"""Reproduce historical audio bugs and verify focused production-code mutants.

Historical RED uses unchanged method bodies from a pinned revision, compiled
against the explicit fake collaborators in audio_lifecycle_before_fix.rs.
Current GREEN/mutation checks compile the actual production modules through
the existing Cargo harness. A separate production-body runner exercises the
coordinator, actual Start, no-token Stop and full cancellation boundary.
None of these layers proves audible hardware behavior.
Compilation errors, zero matching tests, and timeouts are infrastructure errors,
not evidence that a regression test detected the intended defect.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
BASELINE = "bf42308f755749e61b1dbda5bef613d629842d9a"
AUDIO_SOURCE = "src-tauri/src/managers/audio.rs"
MANIFEST = "src-tauri/tests/media-pause/Cargo.toml"
METHODS = (
    "apply_mute",
    "remove_mute",
    "remove_media_pause",
    "stop_microphone_stream",
    "set_state",
    "invalidate_recording_readiness",
    "was_cancelled_since",
    "stop_recording",
    "cancel_recording",
)
HISTORICAL_CASES = (
    (
        "cancel_restores_output_with_always_on_microphone",
        "REGRESSION: cancel left always-on output muted",
    ),
    (
        "cancel_restores_output_before_lazy_microphone_close",
        "REGRESSION: cancel waits for lazy close to unmute",
    ),
    (
        "media_remains_paused_until_capture_stops",
        "REGRESSION: media resumed before tail capture stopped",
    ),
    (
        "late_readiness_cannot_mute_idle_open_microphone",
        "REGRESSION: stale readiness muted idle microphone",
    ),
    (
        "restore_targets_the_original_output",
        "REGRESSION: mute restoration followed the changed default output",
    ),
)

# Each replacement must identify exactly one production statement. Keep this
# list small and tied to the regression assertions, not a coverage percentage.
MUTATIONS = (
    {
        "name": "omit_mute_restore_at_capture_finish",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "        state.restore_mute();\n        self.0.backend.resume();\n        state.recording = None;",
        "after": "        self.0.backend.resume();\n        state.recording = None;",
        "test": "recording_output::tests::cancel_restores_output_with_open_microphone",
    },
    {
        "name": "restore_output_before_tail_and_capture_stop",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "        let result = capture();\n        drop(finish);",
        "after": "        drop(finish);\n        let result = capture();",
        "test": "recording_output::tests::stop_keeps_output_protected_through_tail",
    },
    {
        "name": "accept_late_readiness_after_stop_requested",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "if recording.generation != generation || recording.phase != Phase::AwaitingReady {",
        "after": "if recording.generation != generation {",
        "test": "recording_output::tests::stale_readiness_cannot_mute_after_finish",
    },
    {
        "name": "release_generation_lock_before_mute_side_effect",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "            match self.0.backend.mute() {",
        "after": "            drop(state);\n            let mute_result = self.0.backend.mute();\n            state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());\n            match mute_result {",
        "test": "recording_output::tests::finish_serializes_with_an_in_flight_mute",
    },
    {
        "name": "allow_stale_finish_to_stop_new_capture",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "if recording.generation != generation || recording.phase == Phase::Finishing {",
        "after": "if recording.phase == Phase::Finishing {",
        "test": "recording_output::tests::stale_finish_does_not_restore_new_recording",
    },
    {
        "name": "suppress_mute_when_pause_is_enabled",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "if options.mute && state.muted.len() < MAX_MUTE_LEASES {",
        "after": "if options.mute && !options.pause && state.muted.len() < MAX_MUTE_LEASES {",
        "test": "recording_output::tests::all_four_setting_combinations_are_independent",
    },
    {
        "name": "suppress_new_default_mute_after_unrelated_restore_failure",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "if options.mute && state.muted.len() < MAX_MUTE_LEASES {",
        "after": "if options.mute && state.muted.is_empty() {",
        "test": "recording_output::tests::failed_restore_does_not_disable_new_default_mute",
    },
    {
        "name": "retry_pending_restore_after_capture_start",
        "file": "src-tauri/src/managers/recording_output.rs",
        "before": "        self.0.backend.begin(generation);",
        "after": "        state.restore_mute();\n        self.0.backend.begin(generation);",
        "test": "recording_output::tests::returning_to_retained_output_never_restores_into_a_new_capture",
    },
    {
        "name": "restore_current_default_instead_of_original_output",
        "file": "src-tauri/src/managers/output_mute.rs",
        "before": "self.backend.set_muted(&self.target, false)?;",
        "after": "self.backend.set_muted(&self.backend.default_target()?, false)?;",
        "test": "output_mute::tests::restore_uses_original_output_after_default_changes",
    },
    {
        "name": "treat_unknown_original_mute_as_unmuted",
        "file": "src-tauri/src/managers/output_mute.rs",
        "before": "if backend.is_muted(&target)? {",
        "after": "if backend.is_muted(&target).unwrap_or(false) {",
        "test": "output_mute::tests::unreadable_mute_state_is_not_changed",
    },
)


def sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def run(command: list[str], cwd: Path, log: Path, timeout: int = 120) -> str:
    """Return output only after recording the process result for the receipt."""
    started = time.monotonic()
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        log.write_text(str(error) + "\n")
        raise RuntimeError(f"Infrastructure error running {command[0]}: {error}") from error
    output = result.stdout
    log.write_text(output)
    run.last_result = {
        "command": command,
        "exit_code": result.returncode,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "log": log.name,
    }
    return output


run.last_result = {}


def require_compile(command: list[str], cwd: Path, log: Path) -> dict:
    run(command, cwd, log, timeout=600)
    result = dict(run.last_result)
    if result["exit_code"] != 0:
        raise RuntimeError(f"Compilation failed; this is not a killed mutant: {log}")
    return result


def require_test(
    command: list[str], cwd: Path, log: Path, name: str, passes: bool, marker: str | None = None
) -> dict:
    output = run(command, cwd, log)
    result = dict(run.last_result)
    outcome = "ok" if passes else "FAILED"
    # With --nocapture, the panic/trace may appear between the test-name prefix
    # and its final FAILED/ok line. The named invocation plus the exact summary
    # and (for RED) the failure roster establish that the selected test ran.
    named = re.findall(rf"^test {re.escape(name)} \.\.\.", output, re.MULTILINE)
    summary = (
        r"^test result: ok\. 1 passed; 0 failed; 0 ignored;"
        if passes
        else r"^test result: FAILED\. 0 passed; 1 failed; 0 ignored;"
    )
    summaries = re.findall(summary, output, re.MULTILINE)
    expected_exit = 0 if passes else 101
    if result["exit_code"] != expected_exit or len(named) != 1 or len(summaries) != 1:
        raise RuntimeError(
            f"Expected exactly one {outcome} runtime test {name}; "
            f"zero tests/compilation errors are not regression evidence: {log}"
        )
    if not passes and len(re.findall(rf"^    {re.escape(name)}$", output, re.MULTILINE)) != 1:
        raise RuntimeError(f"Expected the selected runtime test in the failure roster: {log}")
    if marker is not None and marker not in output:
        raise RuntimeError(f"Historical test failed for an unexpected reason: {log}")
    result["test"] = name
    result["expected_result"] = outcome
    return result


def braced_source(source: str, start: int, index: int) -> str:
    """Read one braced Rust block, skipping comments and string/char literals."""
    depth = 0
    began = False
    while index < len(source):
        if source.startswith("//", index):
            end = source.find("\n", index)
            index = len(source) if end < 0 else end + 1
            continue
        if source.startswith("/*", index):
            nesting = 1
            index += 2
            while nesting and index < len(source):
                if source.startswith("/*", index):
                    nesting += 1
                    index += 2
                elif source.startswith("*/", index):
                    nesting -= 1
                    index += 2
                else:
                    index += 1
            continue
        if source[index] == '"':
            index += 1
            while index < len(source):
                if source[index] == "\\":
                    index += 2
                elif source[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
            continue
        char_literal = re.match(r"'(?:\\.|[^'\\])'", source[index:])
        if char_literal:
            index += len(char_literal.group())
            continue
        if source[index] == "{":
            began = True
            depth += 1
        elif source[index] == "}" and began:
            depth -= 1
            if depth == 0:
                return source[start : index + 1]
        index += 1
    raise RuntimeError("Could not delimit a production Rust block")


def function_source(source: str, name: str) -> str:
    """Extract one function without changing any byte inside its body."""
    pattern = rf"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?fn\s+{re.escape(name)}\s*(?:<[^\n(]+>)?\s*\("
    matches = list(re.finditer(pattern, source))
    if len(matches) != 1:
        raise RuntimeError(f"Expected exactly one production function {name}")
    return braced_source(source, matches[0].start(), matches[0].end())


def verify_wiring(sources: dict[str, str]) -> list[str]:
    """Supplement runtime tests with explicit production call-site contracts.

    This intentionally reports a static source check, not execution of Tauri or
    AudioRecordingManager. The historical probe supplies actual old-body RED;
    the shared production controller supplies current runtime GREEN.
    """
    checks = []
    audio = sources[AUDIO_SOURCE]
    for name in ("stop_recording", "cancel_recording"):
        method = function_source(audio, name)
        matches = list(re.finditer(r"\.finish_after_capture\s*\(\s*generation\s*,\s*\|\|\s*", method))
        if len(matches) != 1:
            raise RuntimeError(f"Wiring: {name} must use one shared capture/cleanup boundary")
        match = matches[0]
        capture = braced_source(method, match.start(), match.end())
        remainder = method.replace(capture, "", 1)
        if not re.search(r"rec\.stop\(\)", capture) or re.search(r"rec\.stop\(\)", remainder):
            raise RuntimeError(f"Wiring: {name} must stop the recorder inside its cleanup boundary")
        if name == "stop_recording" and "extra_recording_buffer_ms" not in capture:
            raise RuntimeError("Wiring: capture tail must stay inside the protected boundary")
        if method.find("self.finish_recording_state(generation)") < match.start() + len(capture):
            raise RuntimeError(f"Wiring: {name} may publish Idle before capture cleanup")
        checks.append(f"{name}: capture before cleanup before Idle")

    invalidate = function_source(audio, "invalidate_recording_readiness")
    if ".stop_requested(generation)" not in invalidate or re.search(r"\.(remove|resume|restore_before_exit|finish)\s*\(", invalidate):
        raise RuntimeError("Wiring: readiness invalidation must not restore output into the capture tail")
    checks.append("readiness invalidation retains output protection")

    for name in ("update_mode", "update_selected_device", "restore_output_before_exit"):
        method = function_source(audio, name)
        guard = re.search(r"let (?P<name>_?state) = self\.state\.lock\(\)\.unwrap\(\);", method)
        stream_calls = list(re.finditer(r"self\.(?:stop|start)_microphone_stream\(", method))
        if not guard or not stream_calls or guard.start() > stream_calls[0].start():
            raise RuntimeError(f"Wiring: {name} must serialize stream teardown against recording start")
        drop_guard = method.find(f"drop({guard.group('name')})")
        if 0 <= drop_guard < stream_calls[-1].end():
            raise RuntimeError(f"Wiring: {name} drops its recording guard before stream work finishes")
        checks.append(f"{name}: recording guard covers stream close/open")
    if "self.state.lock()" in function_source(audio, "stop_microphone_stream"):
        raise RuntimeError("Wiring: stream close must not reacquire its caller's recording guard")
    checks.append("stream close does not recursively acquire the recording guard")

    actions = sources["src-tauri/src/actions.rs"]
    impl_marker = "impl ShortcutAction for TranscribeAction"
    impl_start = actions.index(impl_marker)
    transcribe = braced_source(actions, impl_start, impl_start + len(impl_marker))
    stop = function_source(transcribe, "stop")
    capture_stop = stop.find("rm.stop_recording(&binding_id, generation, cancel_generation)")
    stop_cue = stop.find("play_feedback_sound(&ah, SoundType::Stop)")
    if capture_stop < 0 or stop_cue < capture_stop:
        raise RuntimeError("Wiring: the stop cue must follow actual capture stop")
    if re.search(r"\.(remove_mute|remove_media_pause|restore_before_exit)\s*\(", stop):
        raise RuntimeError("Wiring: action Stop must not restore output before manager cleanup")
    request_stop = stop.find(".request_recording_stop(binding_id)")
    async_dispatch = stop.find("tauri::async_runtime::spawn")
    if request_stop < 0 or async_dispatch < 0 or request_stop > async_dispatch:
        raise RuntimeError("Wiring: stop generation must be captured before asynchronous dispatch")
    finish_guard = stop.find("let finish = FinishGuard(")
    if finish_guard < 0 or finish_guard > request_stop:
        raise RuntimeError("Wiring: the completion guard must cover the no-token early return")
    if stop.find("let _guard = finish;") < async_dispatch:
        raise RuntimeError("Wiring: the asynchronous task must own the existing completion guard")
    checks.append("action Stop captures its generation and plays its cue after capture")
    checks.append("action Stop completes the coordinator even when cancellation removed its token")

    windows = sources["src-tauri/src/managers/output_mute_windows.rs"]
    volume = function_source(windows, "with_volume")
    if ".GetDevice(" not in volume or ".GetDefaultAudioEndpoint(" in volume:
        raise RuntimeError("Wiring: Windows restoration must reopen the captured endpoint ID")
    checks.append("Windows endpoint volume reopens captured ID without resolving default")
    return checks


def current_wiring() -> dict:
    paths = (AUDIO_SOURCE, "src-tauri/src/actions.rs", "src-tauri/src/managers/output_mute_windows.rs")
    sources = {path: (ROOT / path).read_text() for path in paths}
    result = {
        "scope": "static source contracts; not runtime AudioRecordingManager or hardware",
        "source_sha256": {path: sha256(source.encode()) for path, source in sources.items()},
        "checks": verify_wiring(sources),
        "rejected_miswirings": [],
    }
    broken = dict(sources)
    cancel = function_source(broken[AUDIO_SOURCE], "cancel_recording")
    changed = cancel.replace(".finish_after_capture(generation, || {", ".close_after_capture(|| {", 1)
    broken[AUDIO_SOURCE] = broken[AUDIO_SOURCE].replace(cancel, changed, 1)
    no_completion = dict(sources)
    no_completion["src-tauri/src/actions.rs"] = no_completion["src-tauri/src/actions.rs"].replace(
        "let finish = FinishGuard(app.clone(), Arc::clone(&tm));", "let finish = ();", 1
    )
    for name, candidate in [
        ("cancel_omits_generation_scoped_finish", broken),
        ("stop_early_return_omits_coordinator_completion", no_completion),
    ]:
        try:
            verify_wiring(candidate)
        except RuntimeError as error:
            result["rejected_miswirings"].append({"name": name, "reason": str(error)})
        else:
            raise RuntimeError(f"Wiring guard accepted an intentionally broken call site: {name}")
    return result


def historical_probe(rustc: str, evidence: Path) -> dict:
    source = subprocess.check_output(
        ["git", "show", f"{BASELINE}:{AUDIO_SOURCE}"], cwd=ROOT, text=True
    )
    template = (ROOT / "scripts/ci/audio_lifecycle_before_fix.rs").read_text()
    extracted = {name: function_source(source, name) for name in ("restore_mute", *METHODS)}
    fixture = template.replace("/* PRODUCTION_FREE_FUNCTIONS */", extracted["restore_mute"])
    fixture = fixture.replace("/* PRODUCTION_METHODS */", "\n\n".join(extracted[name] for name in METHODS))
    generated = evidence / "historical-production-methods.rs"
    generated.write_text(fixture)
    result = {
        "baseline_commit": BASELINE,
        "source_file": AUDIO_SOURCE,
        "source_sha256": sha256(source.encode()),
        "method_sha256": {name: sha256(text.encode()) for name, text in extracted.items()},
        "fixture_sha256": sha256(fixture.encode()),
        "scope": "unchanged production method bodies; fake external collaborators; no hardware",
        "sanity_tests": [],
        "reproduced_regressions": [],
    }
    with tempfile.TemporaryDirectory(prefix="handy-historical-audio-") as temporary:
        binary = Path(temporary) / ("historical-audio.exe" if os.name == "nt" else "historical-audio")
        result["compile"] = require_compile(
            [rustc, "--edition=2021", "--test", str(generated), "-o", str(binary)],
            ROOT,
            evidence / "historical-compile.log",
        )
        for name in ("immediate_on_demand_cancel_restores_output", "user_muted_output_stays_muted"):
            result["sanity_tests"].append(
                require_test(
                    [str(binary), name, "--exact", "--nocapture", "--test-threads=1"],
                    ROOT,
                    evidence / f"historical-sanity-{name}.log",
                    name,
                    passes=True,
                )
            )
        for name, marker in HISTORICAL_CASES:
            result["reproduced_regressions"].append(
                require_test(
                    [str(binary), name, "--exact", "--nocapture", "--test-threads=1"],
                    ROOT,
                    evidence / f"historical-red-{name}.log",
                    name,
                    passes=False,
                    marker=marker,
                )
            )
            print(f"Historical RED reproduced: {name}", flush=True)
    return result


def current_mutations(cargo: str, evidence: Path) -> dict:
    result = {"source_sha256": {}, "baseline_tests": [], "mutations": []}
    with tempfile.TemporaryDirectory(prefix="handy-output-mutations-") as temporary:
        snapshot = Path(temporary)
        for relative in ("src-tauri/src/managers", "src-tauri/tests/media-pause"):
            shutil.copytree(
                ROOT / relative,
                snapshot / relative,
                ignore=shutil.ignore_patterns("target"),
            )
        for path in sorted(snapshot.rglob("*")):
            if path.is_file():
                result["source_sha256"][str(path.relative_to(snapshot))] = sha256(path.read_bytes())
        # Never share the application's active harness target: a concurrent
        # ordinary build must not replace the binary used as mutation evidence.
        # Clean only this small package between variants; dependencies remain
        # cached, and each variant must actually rebuild its production sources.
        target = ROOT / "target/recording-output-regressions"
        command = [cargo, "test", "--locked", "--manifest-path", MANIFEST, "--target-dir", str(target)]

        def clean_harness(label: str) -> dict:
            run(
                [cargo, "clean", "--manifest-path", MANIFEST, "--target-dir", str(target),
                 "--package", "handy-media-pause-tests"],
                snapshot,
                evidence / f"{label}-clean.log",
            )
            cleaned = dict(run.last_result)
            if cleaned["exit_code"] != 0:
                raise RuntimeError("Could not isolate the harness artifacts for a proof variant")
            return cleaned

        result["baseline_clean"] = clean_harness("current-baseline")
        result["baseline_compile"] = require_compile(
            [*command, "--no-run"], snapshot, evidence / "current-baseline-compile.log"
        )
        for name in dict.fromkeys(mutation["test"] for mutation in MUTATIONS):
            result["baseline_tests"].append(
                require_test(
                    [*command, name, "--", "--exact", "--nocapture", "--test-threads=1"],
                    snapshot,
                    evidence / f"current-green-{name.replace('::', '-')}.log",
                    name,
                    passes=True,
                )
            )
        for mutation in MUTATIONS:
            path = snapshot / mutation["file"]
            original = path.read_text()
            if original.count(mutation["before"]) != 1:
                raise RuntimeError(f"Mutation must match exactly once: {mutation['name']}")
            mutated = original.replace(mutation["before"], mutation["after"], 1)
            entry = {
                **mutation,
                "original_sha256": sha256(original.encode()),
                "mutated_sha256": sha256(mutated.encode()),
            }
            try:
                path.write_text(mutated)
                entry["clean"] = clean_harness(f"mutant-{mutation['name']}")
                entry["compile"] = require_compile(
                    [*command, "--no-run"], snapshot, evidence / f"mutant-{mutation['name']}-compile.log"
                )
                entry["test_result"] = require_test(
                    [*command, mutation["test"], "--", "--exact", "--nocapture", "--test-threads=1"],
                    snapshot,
                    evidence / f"mutant-{mutation['name']}-test.log",
                    mutation["test"],
                    passes=False,
                )
                entry["status"] = "killed_by_runtime_assertion"
                result["mutations"].append(entry)
                print(f"Mutation detected by runtime test: {mutation['name']}", flush=True)
            finally:
                path.write_text(original)
    return result


def coordinator_probe(rustc: str, evidence: Path) -> dict:
    """Include runtime coordination proof in the same small CI artifact."""
    output = evidence / "coordinator"
    run(
        [sys.executable, str(ROOT / "scripts/ci/coordinator_regressions.py"),
         "--rustc", rustc, "--output", str(output)],
        ROOT,
        evidence / "coordinator-runner.log",
        timeout=600,
    )
    runner = dict(run.last_result)
    if runner["exit_code"] != 0:
        raise RuntimeError("Coordinator runtime/mutation proof failed; see coordinator-runner.log")
    receipt = json.loads((output / "receipt.json").read_text())
    if receipt.get("status") != "all_expected_results_verified":
        raise RuntimeError("Coordinator receipt did not verify all expected results")
    print("Coordinator runtime and mutation evidence verified", flush=True)
    return {"runner": runner, "receipt_path": "coordinator/receipt.json", **receipt}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output", type=Path,
        default=Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())) / "handy-recording-output-evidence",
    )
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--rustc", default="rustc")
    parser.add_argument("--historical-only", action="store_true")
    args = parser.parse_args()
    evidence = args.output.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    receipt = {
        "schema_version": 1,
        "checked_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "working_tree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)),
        "historical_only": args.historical_only,
        "hardware_audio_verified": False,
        "status": "incomplete",
    }
    try:
        receipt["historical"] = historical_probe(args.rustc, evidence)
        if not args.historical_only:
            receipt["wiring"] = current_wiring()
            receipt["current"] = current_mutations(args.cargo, evidence)
            receipt["coordinator"] = coordinator_probe(args.rustc, evidence)
            for field in ("checked_commit", "working_tree_dirty"):
                if receipt["coordinator"][field] != receipt[field]:
                    raise RuntimeError(f"Repository state changed during coordinator proof: {field}")
        receipt["status"] = "historical_red_verified" if args.historical_only else "all_expected_results_verified"
        return 0
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        receipt["error"] = str(error)
        print(f"Regression evidence incomplete: {error}", file=sys.stderr)
        return 1
    finally:
        destination = evidence / "receipt.json"
        destination.write_text(json.dumps(receipt, indent=2) + "\n")
        print(f"Evidence receipt: {destination}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
