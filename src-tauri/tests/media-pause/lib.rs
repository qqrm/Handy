//! Compiles and tests the actual production module without requiring
//! unrelated speech-model/GPU SDKs. The application build remains a separate gate.
#![cfg(any(test, all(target_os = "macos", feature = "quicktime-fixture")))]

#[path = "../../src/managers/media_pause.rs"]
mod media_pause;

#[cfg(target_os = "macos")]
#[path = "../../src/managers/media_pause_macos_fixture.rs"]
mod quicktime_fixture;
