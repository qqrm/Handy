// Compile the production media, output and lifecycle modules directly.
#![allow(dead_code)]

#[path = "../../src/coordinator_cleanup.rs"]
mod coordinator_cleanup;
#[path = "../../src/media_control.rs"]
mod media_control;
#[path = "../../src/managers/output_mute.rs"]
mod output_mute;
#[path = "../../src/managers/recording_output.rs"]
mod recording_output;
