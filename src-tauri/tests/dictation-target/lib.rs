// Exercise the production module without microphone, model, or Tauri builds.
#[path = "../../src/dictation_target/mod.rs"]
pub mod dictation_target;

#[cfg(all(windows, test))]
mod native_fixture;

#[cfg(all(windows, test))]
mod edge_fixture;
