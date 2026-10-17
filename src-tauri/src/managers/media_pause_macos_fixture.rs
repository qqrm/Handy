//! C entry points for the disposable native fixture. They invoke the production
//! controller and permission API, not a second implementation of media policy.
use crate::media_pause::{request_quicktime_permission, MediaPauseController};
use std::ffi::c_void;

fn isolated() -> bool {
    [("GITHUB_ACTIONS", "true"), ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "macOS"), ("HANDY_RUN_ISOLATED_QUICKTIME_TESTS", "1")]
        .iter().all(|(key, value)| std::env::var(key).is_ok_and(|actual| actual == *value))
}
#[no_mangle]
pub extern "C" fn handy_fixture_permission() -> i32 {
    if !isolated() { return -1; }
    match request_quicktime_permission() { Ok(true) => 1, Ok(false) => 0, Err(_) => -1 }
}
#[no_mangle]
pub extern "C" fn handy_fixture_new() -> *mut c_void {
    if !isolated() { return std::ptr::null_mut(); }
    Box::into_raw(Box::new(MediaPauseController::new())).cast()
}
#[no_mangle]
pub extern "C" fn handy_fixture_begin(raw: *mut c_void, generation: u64) {
    if !isolated() || raw.is_null() { return; }
    let controller = unsafe { &*raw.cast::<MediaPauseController>() };
    controller.begin(generation);
    controller.apply(generation, true, true);
}
#[no_mangle]
pub extern "C" fn handy_fixture_end(raw: *mut c_void) {
    if !isolated() || raw.is_null() { return; }
    unsafe { &*raw.cast::<MediaPauseController>() }.remove();
}
#[no_mangle]
pub extern "C" fn handy_fixture_destroy(raw: *mut c_void) {
    if !isolated() || raw.is_null() { return; }
    let controller = unsafe { Box::from_raw(raw.cast::<MediaPauseController>()) };
    controller.restore_before_exit();
}
