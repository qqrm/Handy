use crate::dictation_target::{TargetCapture, TargetError, TargetSnapshot};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::{EM_REPLACESEL, EM_SETSEL};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, GetWindowLongW, PeekMessageW, PostThreadMessageW, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowLongW, ShowWindow, UnregisterClassW, ES_AUTOHSCROLL, ES_PASSWORD,
    ES_READONLY, GWL_STYLE, MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_GETTEXT, WM_GETTEXTLENGTH, WM_SETTEXT, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE,
};

const WAKE: u32 = WM_APP + 0x71;
const WAIT: Duration = Duration::from_secs(10);
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) enum Action {
    FocusA(usize, usize),
    FocusB,
    SetA(String),
    ReplaceA(String),
    ProtectA(i32),
    ReadA,
    ReadB,
    DestroyA,
    Stop,
}

struct Request {
    action: Action,
    reply: mpsc::Sender<String>,
}

pub(crate) struct Fixture {
    sender: mpsc::Sender<Request>,
    thread_id: u32,
    worker: Option<thread::JoinHandle<()>>,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    pub(crate) fn start() -> Self {
        let serial = FIXTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (sender, receiver) = mpsc::channel();
        let (ready, result) = mpsc::channel();
        let worker = thread::spawn(move || fixture_thread(receiver, ready));
        Self {
            sender,
            thread_id: result.recv_timeout(WAIT).expect("fixture ready"),
            worker: Some(worker),
            _serial: serial,
        }
    }

    pub(crate) fn call(&self, action: Action) -> String {
        let (reply, result) = mpsc::channel();
        self.sender.send(Request { action, reply }).unwrap();
        unsafe { PostThreadMessageW(self.thread_id, WAKE, WPARAM(0), LPARAM(0)) }.unwrap();
        result.recv_timeout(WAIT).expect("fixture operation")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let (reply, result) = mpsc::channel();
        if self
            .sender
            .send(Request {
                action: Action::Stop,
                reply,
            })
            .is_ok()
            && unsafe { PostThreadMessageW(self.thread_id, WAKE, WPARAM(0), LPARAM(0)) }.is_ok()
        {
            let _ = result.recv_timeout(WAIT);
        }
        if let Some(worker) = self.worker.take() {
            if let Err(error) = worker.join() {
                // Preserve the original setup/assertion failure instead of
                // aborting the whole harness with a second panic in Drop.
                if !thread::panicking() {
                    std::panic::resume_unwind(error);
                }
            }
        }
    }
}

struct FixtureWindows {
    top_a: HWND,
    top_b: HWND,
    instance: HINSTANCE,
}

impl Drop for FixtureWindows {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.top_a) };
        let _ = unsafe { DestroyWindow(self.top_b) };
        let _ = unsafe { UnregisterClassW(w!("HandyPinnedTargetFixture"), Some(self.instance)) };
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn set_value(hwnd: HWND, value: &str) {
    let text = wide(value);
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETTEXT,
            Some(WPARAM(0)),
            Some(LPARAM(text.as_ptr() as isize)),
        )
    };
}

fn read_value(hwnd: HWND) -> String {
    let length = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None) }.0 as usize;
    let mut value = vec![0_u16; length + 1];
    let copied = unsafe {
        SendMessageW(
            hwnd,
            WM_GETTEXT,
            Some(WPARAM(value.len())),
            Some(LPARAM(value.as_mut_ptr() as isize)),
        )
    }
    .0 as usize;
    String::from_utf16(&value[..copied]).unwrap()
}

fn focus(top: HWND, edit: HWND) {
    let _ = unsafe { ShowWindow(top, SW_SHOW) };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let _ = unsafe { SetForegroundWindow(top) };
        let _ = unsafe { SetFocus(Some(edit)) };
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe { DispatchMessageW(&message) };
        }
        if unsafe { GetForegroundWindow() } == top && unsafe { GetFocus() } == edit {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture could not acquire real foreground/focus: expected {top:?}/{edit:?}, actual {:?}/{:?}",
            unsafe { GetForegroundWindow() },
            unsafe { GetFocus() }
        );
        thread::sleep(Duration::from_millis(25));
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn fixture_thread(receiver: mpsc::Receiver<Request>, ready: mpsc::Sender<u32>) {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.unwrap().0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: w!("HandyPinnedTargetFixture"),
        ..Default::default()
    };
    assert_ne!(unsafe { RegisterClassW(&class) }, 0);
    let make_window = |title: PCWSTR, visible: bool| {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("HandyPinnedTargetFixture"),
                title,
                WS_OVERLAPPEDWINDOW | if visible { WS_VISIBLE } else { WINDOW_STYLE(0) },
                100,
                100,
                360,
                180,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .unwrap()
    };
    let top_a = make_window(w!("Handy fixture document A"), true);
    let top_b = make_window(w!("Handy fixture document B"), false);
    let _windows = FixtureWindows {
        top_a,
        top_b,
        instance,
    };
    let make_edit = |parent: HWND| {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("Edit"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                10,
                10,
                300,
                80,
                Some(parent),
                None,
                Some(instance),
                None,
            )
        }
        .unwrap()
    };
    let edit_a = make_edit(top_a);
    let edit_b = make_edit(top_b);
    set_value(edit_a, "before after");
    set_value(edit_b, "untouched B");
    ready.send(unsafe { GetCurrentThreadId() }).unwrap();
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        unsafe { DispatchMessageW(&message) };
        while let Ok(request) = receiver.try_recv() {
            let response = match request.action {
                Action::FocusA(start, end) => {
                    focus(top_a, edit_a);
                    unsafe {
                        SendMessageW(
                            edit_a,
                            EM_SETSEL,
                            Some(WPARAM(start)),
                            Some(LPARAM(end as isize)),
                        )
                    };
                    String::new()
                }
                Action::FocusB => {
                    focus(top_b, edit_b);
                    String::new()
                }
                Action::SetA(value) => {
                    set_value(edit_a, &value);
                    String::new()
                }
                Action::ReplaceA(value) => {
                    let text = wide(&value);
                    unsafe {
                        SendMessageW(
                            edit_a,
                            EM_REPLACESEL,
                            Some(WPARAM(1)),
                            Some(LPARAM(text.as_ptr() as isize)),
                        )
                    };
                    String::new()
                }
                Action::ProtectA(flag) => {
                    let style = unsafe { GetWindowLongW(edit_a, GWL_STYLE) };
                    unsafe {
                        SetWindowLongW(
                            edit_a,
                            GWL_STYLE,
                            (style & !(ES_READONLY | ES_PASSWORD)) | flag,
                        )
                    };
                    String::new()
                }
                Action::ReadA => read_value(edit_a),
                Action::ReadB => read_value(edit_b),
                Action::DestroyA => {
                    unsafe { DestroyWindow(edit_a) }.unwrap();
                    String::new()
                }
                Action::Stop => {
                    let _ = request.reply.send(String::new());
                    return;
                }
            };
            let _ = request.reply.send(response);
        }
    }
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn native_control_lifecycle_fixture() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusA(7, 7));
    let mut target = TargetSnapshot::capture().expect("capture real edit A");
    fixture.call(Action::FocusB);
    assert!(
        target
            .insert("dictation ")
            .expect("background insertion")
            .verified
    );
    assert_eq!(fixture.call(Action::ReadA), "before dictation after");
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    assert!(!target.is_focused_target().unwrap());
    assert_eq!(target.insert("duplicate"), Err(TargetError::AlreadyUsed));

    fixture.call(Action::FocusA(17, 17));
    assert!(
        !target.is_focused_target().unwrap(),
        "background insertion cannot arm auto-submit"
    );
    fixture.call(Action::FocusA(0, 0));
    assert!(
        !target.is_focused_target().unwrap(),
        "moved caret must block submit"
    );
    drop(target);

    fixture.call(Action::FocusA(0, 0));
    let mut target = TargetSnapshot::capture().unwrap();
    assert_eq!(
        target.insert_if_active("cancelled", Arc::new(AtomicBool::new(false))),
        Err(TargetError::Unavailable)
    );
    assert_eq!(fixture.call(Action::ReadA), "before dictation after");
    drop(target);

    fixture.call(Action::SetA("new document".into()));
    fixture.call(Action::FocusA(0, 3));
    let mut target = TargetSnapshot::capture().expect("selection capture");
    assert!(target.insert("replacement").unwrap().verified);
    assert_eq!(fixture.call(Action::ReadA), "replacement document");
    drop(target);

    fixture.call(Action::FocusA(0, 0));
    let mut target = TargetSnapshot::capture().unwrap();
    fixture.call(Action::SetA("changed by the application".into()));
    assert_eq!(target.insert("dictation"), Err(TargetError::Changed));
    assert_eq!(fixture.call(Action::ReadA), "changed by the application");
    drop(target);

    fixture.call(Action::ProtectA(ES_READONLY));
    assert!(matches!(
        TargetSnapshot::capture(),
        Err(TargetError::Protected)
    ));
    fixture.call(Action::ProtectA(ES_PASSWORD));
    assert!(matches!(
        TargetSnapshot::capture(),
        Err(TargetError::Protected)
    ));
    fixture.call(Action::ProtectA(0));

    fixture.call(Action::FocusA(0, 0));
    let mut target = TargetSnapshot::capture().unwrap();
    fixture.call(Action::DestroyA);
    fixture.call(Action::FocusB);
    assert_eq!(
        target.insert("must not go to B"),
        Err(TargetError::Unavailable)
    );
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn foreground_readback_fixture() {
    let fixture = Fixture::start();
    fixture.call(Action::SetA("prefix suffix".into()));
    fixture.call(Action::FocusA(7, 7));
    // Force only capability selection, not the provider or the delivery code:
    // all reads still go through the real Edit control's UIA TextPattern. This
    // does not claim that a particular browser's contenteditable was tested.
    let mut target = TargetSnapshot::capture_foreground_for_test().unwrap();
    assert!(target.requires_foreground_paste());
    let active = Arc::new(AtomicBool::new(true));
    target
        .prepare_foreground_insert("insert ", active.clone())
        .unwrap();
    assert!(target.foreground_insert_is_ready().unwrap());
    fixture.call(Action::FocusB);
    assert!(!target.foreground_insert_is_ready().unwrap());
    fixture.call(Action::FocusA(8, 8));
    assert!(
        !target.foreground_insert_is_ready().unwrap(),
        "changed caret blocks paste"
    );
    fixture.call(Action::FocusA(7, 7));
    assert!(target.foreground_insert_is_ready().unwrap());

    // Simulate an already-issued paste being processed asynchronously by the
    // target's event loop. Confirmation must wait for readback, never re-paste.
    let sender = fixture.sender.clone();
    let thread_id = fixture.thread_id;
    let delayed_input = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        let (reply, result) = mpsc::channel();
        sender
            .send(Request {
                action: Action::ReplaceA("insert ".into()),
                reply,
            })
            .unwrap();
        unsafe { PostThreadMessageW(thread_id, WAKE, WPARAM(0), LPARAM(0)) }.unwrap();
        result.recv_timeout(WAIT).unwrap();
    });
    assert!(target.confirm_foreground_insert().unwrap().verified);
    delayed_input.join().unwrap();
    assert_eq!(fixture.call(Action::ReadA), "prefix insert suffix");
    assert_eq!(fixture.call(Action::ReadB), "untouched B");
    assert!(target.is_focused_target().unwrap());
    assert_eq!(
        target.confirm_foreground_insert(),
        Err(TargetError::AlreadyUsed)
    );
    drop(target);

    fixture.call(Action::SetA("unchanged".into()));
    fixture.call(Action::FocusA(9, 9));
    let mut target = TargetSnapshot::capture_foreground_for_test().unwrap();
    target
        .prepare_foreground_insert(" tail", active.clone())
        .unwrap();
    active.store(false, std::sync::atomic::Ordering::Release);
    assert!(!target.foreground_insert_is_ready().unwrap());
    assert_eq!(fixture.call(Action::ReadA), "unchanged");
    drop(target);

    fixture.call(Action::FocusA(9, 9));
    let mut target = TargetSnapshot::capture_foreground_for_test().unwrap();
    target
        .prepare_foreground_insert(" tail", Arc::new(AtomicBool::new(true)))
        .unwrap();
    fixture.call(Action::SetA("application changed this".into()));
    assert_eq!(
        target.confirm_foreground_insert(),
        Err(TargetError::Uncertain)
    );
    assert_eq!(fixture.call(Action::ReadA), "application changed this");
}

#[test]
#[ignore = "requires a Windows desktop; run explicitly in the Windows safety job"]
fn deferred_capture_fixture() {
    let fixture = Fixture::start();
    fixture.call(Action::FocusA(7, 7));
    let (entered, acquisition_started) = mpsc::channel();
    let (release, acquisition_release) = mpsc::channel();
    let started = std::time::Instant::now();
    let capture = TargetCapture::begin_blocked_for_test(entered, acquisition_release);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "begin must not wait for provider acquisition"
    );
    acquisition_started.recv_timeout(WAIT).unwrap();
    release.send(()).unwrap();
    // Real UIA acquisition completes in the background while ASR is represented
    // by a longer wait. An already-ready result must survive its old deadline.
    thread::sleep(Duration::from_millis(4100));
    let mut target = capture
        .resolve()
        .expect("completed capture survives long ASR");
    assert!(target.insert("dictation ").unwrap().verified);
    assert_eq!(fixture.call(Action::ReadA), "before dictation after");
}
