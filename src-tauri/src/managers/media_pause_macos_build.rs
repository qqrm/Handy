//! Shared by the application and the isolated fixture: compile the same shim.
pub fn build(source: &std::path::Path, testing: bool) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    use std::process::Command;
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => panic!("Unsupported QuickTime target: {other:?}"),
    };
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = output.join("media_pause_macos.o");
    let archive = output.join("libhandy_quicktime.a");
    let mut clang = Command::new("xcrun");
    clang.args(["--sdk", "macosx", "clang", "-arch", arch, "-fobjc-arc", "-fblocks", "-Wall", "-Wextra", "-Werror"]);
    clang.arg(if arch == "arm64" { "-mmacosx-version-min=11.0" } else { "-mmacosx-version-min=10.15" }).arg("-c");
    if testing {
        clang.arg("-DHANDY_QUICKTIME_FIXTURE=1");
    }
    assert!(clang.arg(source).arg("-o").arg(&object).status().expect("Run clang").success(), "QuickTime shim compilation failed");
    assert!(Command::new("xcrun").args(["libtool", "-static", "-o"]).arg(&archive).arg(&object).status().expect("Run libtool").success(), "QuickTime shim archive failed");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rustc-link-search=native={}", output.display());
    println!("cargo:rustc-link-lib=static=handy_quicktime");
    println!("cargo:rustc-link-lib=framework=Cocoa");
    println!("cargo:rustc-link-lib=framework=CoreServices");
}
