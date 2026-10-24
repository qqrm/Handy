// This harness imports production modules directly; it does not copy their logic.
// Public adapter entry points are compiled but require live players to execute.
#![allow(dead_code)]

#[path = "../../src/media_control_windows_batch.rs"]
mod windows_batch;

#[cfg(target_os = "windows")]
#[path = "../../src/media_control_windows.rs"]
mod platform_windows;
