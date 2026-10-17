pub mod audio;
pub mod gguf_meta;
pub mod history;
mod media_pause;
#[cfg(target_os = "macos")]
pub(crate) use media_pause::request_quicktime_permission;
pub mod model;
pub mod model_capabilities;
pub mod transcription;
