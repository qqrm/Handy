//! Compiles and tests the actual production module without requiring
//! unrelated speech-model/GPU SDKs. The application build remains a separate gate.
#![cfg(test)]

#[path = "../../src/managers/media_pause.rs"]
mod media_pause;
