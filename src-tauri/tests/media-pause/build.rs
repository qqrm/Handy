#[path = "../../src/managers/media_pause_macos_build.rs"]
mod media_pause_macos_build;

fn main() {
    media_pause_macos_build::build(
        std::path::Path::new("../../src/managers/media_pause_macos.m"),
        std::env::var_os("CARGO_FEATURE_QUICKTIME_FIXTURE").is_some(),
    );
}
