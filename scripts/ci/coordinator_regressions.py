#!/usr/bin/env python3
"""Exercise production stop/cancel/start coordination with gated external services.

Compiles the whole coordinator module and its real unit tests, unchanged audio
manager lifecycle methods, TranscribeAction::start, centralized cancellation,
FinishGuard, Stop's token-acquisition entry and the output selection policy.
After a successful Stop token, an explicitly fake ASR boundary records the
frozen output mode and original post-processing action, then completes real
capture cleanup. Tauri, microphone/model/UI services and reminder/output hooks
are fakes; this is not speech recognition, native delivery, hardware or a full
Tauri application test. Source mutations run only in temporary copies.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from recording_output_regressions import (
    ROOT, braced_source, function_source, require_compile, require_test, run, sha256,
)


COORDINATOR = "src-tauri/src/transcription_coordinator.rs"
AUDIO = "src-tauri/src/managers/audio.rs"
ACTIONS = "src-tauri/src/actions.rs"
UTILS = "src-tauri/src/utils.rs"
OUTPUT_POLICY = "src-tauri/src/dictation_output/policy.rs"
RECORDING_SCOPE = "src-tauri/src/shortcut/recording_scope.rs"
AUDIO_METHODS = (
    "set_state", "try_start_recording", "request_recording_stop",
    "invalidate_recording_readiness", "is_recording_readiness_current",
    "cancel_generation", "was_cancelled_since", "stop_recording",
    "finish_recording_state", "is_recording", "cancel_recording",
)
RUNTIME_CASES = (
    "transcription_coordinator::runtime_tests::no_token_stop_waits_for_real_capture_and_full_cancel_cleanup",
    "transcription_coordinator::runtime_tests::actual_failed_start_does_not_claim_an_older_active_capture",
    "transcription_coordinator::runtime_tests::repeated_cancel_uses_one_worker_and_discards_queued_restart",
    "transcription_coordinator::runtime_tests::clipboard_stop_freezes_policy_before_instant_recognition_and_preserves_postprocess",
    "transcription_coordinator::runtime_tests::clipboard_shortcut_is_inert_while_idle_and_after_normal_stop",
    "transcription_coordinator::runtime_tests::clipboard_stop_discards_the_old_release_before_a_fast_new_recording",
    "transcription_coordinator::runtime_tests::reminder_hooks_distinguish_holding_tapping_and_cancellation",
    "transcription_coordinator::runtime_tests::queued_old_recording_shortcuts_cannot_stop_or_cancel_the_next_recording",
)
MUTATIONS = (
    {
        "name": "queued_recording_shortcut_loses_its_scope",
        "before": "&& crate::shortcut::recording_shortcut_scope(app) == Some(scope)",
        "after": "&& { let _ = scope; true }",
        "test": RUNTIME_CASES[7],
        "marker": "REGRESSION: a queued old recording shortcut affected the next recording",
    },
    {
        "name": "pipeline_completion_ignores_cancellation_cleanup",
        "before": "if self.processing_pending || self.cancellation_pending {",
        "after": "if self.processing_pending {",
        "test": RUNTIME_CASES[0],
        "marker": "REGRESSION: no-token Stop published Idle during capture cleanup",
    },
    {
        "name": "start_reports_the_previous_recording_as_success",
        "before": "let recording = action.start(app, binding_id, hotkey_string);",
        "after": "action.start(app, binding_id, hotkey_string);\n"
                 "    let recording = tauri::Manager::state::<std::sync::Arc<"
                 "crate::managers::audio::AudioRecordingManager>>(app).is_recording();",
        "test": RUNTIME_CASES[1],
        "marker": "REGRESSION: rejected Start claimed the old recording as success",
    },
    {
        "name": "cancel_notifies_before_its_cleanup",
        "before": "let _guard = finish;\n        cancel();",
        "after": "drop(finish);\n        cancel();",
        "test": RUNTIME_CASES[0],
        "marker": "REGRESSION: cancellation completion preceded its cleanup",
    },
    {
        "name": "repeated_cancel_starts_another_worker",
        "before": "!std::mem::replace(&mut self.cancellation_pending, true)",
        "after": "{ self.cancellation_pending = true; true }",
        "test": "transcription_coordinator::tests::repeated_cancellation_shares_cleanup_and_discards_the_abandoned_press",
        "marker": "an existing worker already owns cleanup",
    },
    {
        "name": "clipboard_completion_inserts_instead_of_copying",
        "before": "} => stop(app, &binding_id, &hotkey_string, OutputMode::ClipboardOnly),",
        "after": "} => stop(app, &binding_id, &hotkey_string, OutputMode::Insert),",
        "test": RUNTIME_CASES[3],
        "marker": "REGRESSION: clipboard completion changed its output policy or original action",
    },
    {
        "name": "clipboard_completion_keeps_the_previous_release",
        "before": "let binding_id = binding_id.clone();\n        self.pending_release = None;",
        "after": "let binding_id = binding_id.clone();",
        "test": RUNTIME_CASES[5],
        "marker": "REGRESSION: the previous release swallowed the next recording's Start",
    },
    {
        "name": "cancellation_omits_recording_reminder_stop",
        "before": "Command::Cancel => {\n            crate::recording_reminders::stop(app);",
        "after": "Command::Cancel => {",
        "test": RUNTIME_CASES[6],
        "marker": "REGRESSION: cancellation left recording reminders running",
    },
)


def block(source: str, marker: str) -> str:
    if source.count(marker) != 1:
        raise RuntimeError(f"Expected one production block: {marker}")
    start = source.index(marker)
    return braced_source(source, start, start + len(marker))


def build_fixture(sources: dict[str, str]) -> tuple[str, dict[str, str]]:
    audio, actions = sources[AUDIO], sources[ACTIONS]
    transcribe = block(actions, "impl ShortcutAction for TranscribeAction")
    stop = function_source(transcribe, "stop")
    stop_boundary = "        let stop_time = Instant::now();"
    if stop.count(stop_boundary) != 1:
        raise RuntimeError("Could not delimit the production no-token Stop entry")
    stop_entry = stop[:stop.index(stop_boundary)]
    if ".request_recording_stop(binding_id)" not in stop_entry:
        raise RuntimeError("Stop entry omitted its token acquisition")

    fragments = {
        "PRODUCTION_COORDINATOR": sources[COORDINATOR],
        "PRODUCTION_OUTPUT_POLICY": sources[OUTPUT_POLICY],
        "PRODUCTION_RECORDING_SCOPE": sources[RECORDING_SCOPE],
        "PRODUCTION_RECORDING_TYPES": "\n\n".join((
            "#[derive(Clone, Debug)]\n" + block(audio, "pub enum RecordingState"),
            "#[derive(Clone, Debug)]\n" + block(audio, "pub enum MicrophoneMode"),
            block(audio, "pub struct RecordingReadiness"),
            block(audio, "impl RecordingReadiness"),
        )),
        "PRODUCTION_AUDIO_METHODS": "\n\n".join(
            function_source(audio, name) for name in AUDIO_METHODS
        ),
        "PRODUCTION_CANCEL_FUNCTIONS": "\n\n".join(
            function_source(sources[UTILS], name)
            for name in ("cancel_current_operation", "finish_cancellation")
        ),
        "PRODUCTION_FINISH_GUARD": actions[
            actions.index("struct FinishGuard("):actions.index("// Shortcut Action Trait")
        ],
        "PRODUCTION_ACTION_TRAIT": block(actions, "pub trait ShortcutAction"),
        "PRODUCTION_ACTION_START": function_source(transcribe, "start"),
        "PRODUCTION_STOP_FUNCTION": stop_entry +
            "        finish_at_fake_asr_boundary(app, binding_id, generation, self.post_process, output);\n    }",
    }
    fixture = (ROOT / "scripts/ci/coordinator_lifecycle_fixture.rs").read_text()
    for name, fragment in fragments.items():
        marker = f"/* {name} */"
        if fixture.count(marker) != 1:
            raise RuntimeError(f"Expected one fixture marker: {marker}")
        fixture = fixture.replace(marker, fragment)
    return fixture, {name: sha256(value.encode()) for name, value in fragments.items()}


def verify(rustc: str, evidence: Path) -> dict:
    sources = {
        path: (ROOT / path).read_text()
        for path in (COORDINATOR, AUDIO, ACTIONS, UTILS, OUTPUT_POLICY, RECORDING_SCOPE)
    }
    fixture, fragment_hashes = build_fixture(sources)
    generated = evidence / "coordinator-production-bodies.rs"
    generated.write_text(fixture)
    result = {
        "scope": __doc__,
        "source_sha256": {path: sha256(value.encode()) for path, value in sources.items()},
        "production_fragment_sha256": fragment_hashes,
        "fixture_sha256": sha256(fixture.encode()),
        "runtime_cases": [],
        "mutations": [],
    }
    with tempfile.TemporaryDirectory(prefix="handy-coordinator-regressions-") as temporary:
        binary = Path(temporary) / ("coordinator.exe" if os.name == "nt" else "coordinator")
        result["compile"] = require_compile(
            [rustc, "--edition=2021", "--test", str(generated), "-o", str(binary)],
            ROOT, evidence / "coordinator-compile.log",
        )
        output = run([str(binary), "--test-threads=1"], ROOT, evidence / "coordinator-all-tests.log")
        result["all_tests"] = dict(run.last_result)
        expected = (
            sources[COORDINATOR].count("#[test]")
            + sources[OUTPUT_POLICY].count("#[test]")
            + sources[RECORDING_SCOPE].count("#[test]")
            + len(RUNTIME_CASES)
        )
        summary = rf"^test result: ok\. {expected} passed; 0 failed; 0 ignored;"
        if result["all_tests"]["exit_code"] != 0 or not re.search(summary, output, re.MULTILINE):
            raise RuntimeError(f"Expected {expected} production coordinator/runtime tests: {evidence / 'coordinator-all-tests.log'}")
        result["all_tests"]["passed"] = expected
        print(f"Coordinator production/runtime tests passed: {expected}", flush=True)
        for index, name in enumerate(RUNTIME_CASES):
            result["runtime_cases"].append(require_test(
                [str(binary), name, "--exact", "--nocapture", "--test-threads=1"],
                ROOT, evidence / f"coordinator-runtime-{index}.log", name, passes=True,
            ))

        for mutation in MUTATIONS:
            changed = dict(sources)
            if changed[COORDINATOR].count(mutation["before"]) != 1:
                raise RuntimeError(f"Mutation target drifted: {mutation['name']}")
            changed[COORDINATOR] = changed[COORDINATOR].replace(mutation["before"], mutation["after"], 1)
            mutated_fixture, _ = build_fixture(changed)
            mutated = evidence / f"coordinator-mutant-{mutation['name']}.rs"
            mutated.write_text(mutated_fixture)
            compile_result = require_compile(
                [rustc, "--edition=2021", "--test", str(mutated), "-o", str(binary)],
                ROOT, evidence / f"coordinator-mutant-{mutation['name']}-compile.log",
            )
            test_result = require_test(
                [str(binary), mutation["test"], "--exact", "--nocapture", "--test-threads=1"],
                ROOT, evidence / f"coordinator-mutant-{mutation['name']}-test.log",
                mutation["test"], passes=False, marker=mutation["marker"],
            )
            result["mutations"].append({"name": mutation["name"], "compile": compile_result, "test": test_result})
            print(f"Coordinator mutation detected by runtime test: {mutation['name']}", flush=True)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()) / "handy-coordinator-evidence")
    parser.add_argument("--rustc", default="rustc")
    args = parser.parse_args()
    evidence = args.output.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    receipt = {
        "schema_version": 1,
        "checked_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "working_tree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)),
        "hardware_audio_verified": False,
        "status": "incomplete",
    }
    try:
        receipt["coordinator"] = verify(args.rustc, evidence)
        receipt["status"] = "all_expected_results_verified"
        return 0
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        receipt["error"] = str(error)
        print(f"Coordinator regression evidence incomplete: {error}", file=sys.stderr)
        return 1
    finally:
        destination = evidence / "receipt.json"
        destination.write_text(json.dumps(receipt, indent=2) + "\n")
        print(f"Coordinator evidence receipt: {destination}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
