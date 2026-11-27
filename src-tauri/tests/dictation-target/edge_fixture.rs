//! One real browser/provider check; no mock DOM, remote debugging, or user profile.
use crate::dictation_target::{TargetCapture, TargetError};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, BOOL, BSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::{
    CreateProcessW, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED,
    PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationValuePattern, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TreeScope_Descendants, UIA_NamePropertyId, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VK_CONTROL, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, EnumWindows, GetWindowTextW, GetWindowThreadProcessId,
    PostMessageW, SetForegroundWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE,
};

const TARGET_NAME: &str = "Handy intended editor";
const OTHER_NAME: &str = "Handy untouched editor";

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct ComApartment;
impl ComApartment {
    fn enter() -> Self {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .unwrap();
        Self
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Browser {
    job: Option<OwnedHandle>,
    process: OwnedHandle,
    process_id: u32,
    directory: PathBuf,
    title: String,
    window: Option<HWND>,
}

impl Browser {
    fn launch() -> Self {
        let edge = ["PROGRAMFILES(X86)", "PROGRAMFILES"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|root| PathBuf::from(root).join("Microsoft/Edge/Application/msedge.exe"))
            .find(|path| path.is_file())
            .expect("Microsoft Edge is required for the real browser fixture");
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(format!("handy-edge-target-{unique}"));
        std::fs::create_dir(&directory).unwrap();
        let profile = directory.join("isolated-profile");
        let page = directory.join("fixture.html");
        let title = format!("Handy browser fixture {unique}");
        std::fs::write(&page, format!(r#"<!doctype html>
<html lang="en"><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'none'">
<title>{title}</title><style>body{{font:20px sans-serif;padding:30px}}[contenteditable],textarea{{display:block;width:600px;height:100px;margin-bottom:30px;border:1px solid #333}}</style>
<div id="target" contenteditable="true" role="textbox" aria-label="{TARGET_NAME}">before after</div>
<textarea aria-label="{OTHER_NAME}">untouched B</textarea>
<script>addEventListener('load',()=>{{const e=document.getElementById('target');e.focus();const r=document.createRange();r.setStart(e.firstChild,7);r.collapse(true);const s=getSelection();s.removeAllRanges();s.addRange(r);}});</script>
</html>"#)).unwrap();
        let command = format!(
            "\"{}\" --user-data-dir=\"{}\" --app=\"{}\" --force-renderer-accessibility --no-first-run --no-default-browser-check --disable-background-networking --disable-component-update --disable-sync --disable-extensions --disable-default-apps --proxy-server=http://127.0.0.1:9 --proxy-bypass-list=\"<-loopback>\" --window-size=800,600",
            edge.display(), profile.display(), file_url(&page)
        );
        let application = wide(&edge.to_string_lossy());
        let mut command = wide(&command);
        let job = OwnedHandle(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.unwrap());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .unwrap();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_SUSPENDED,
                None,
                PCWSTR::null(),
                &startup,
                &mut process,
            )
        }
        .unwrap();
        let process_handle = OwnedHandle(process.hProcess);
        let initial_thread = OwnedHandle(process.hThread);
        if let Err(error) = unsafe { AssignProcessToJobObject(job.0, process_handle.0) } {
            let _ = unsafe { TerminateProcess(process_handle.0, 1) };
            panic!("cannot isolate Edge process tree: {error}");
        }
        assert_ne!(unsafe { ResumeThread(initial_thread.0) }, u32::MAX);
        Self {
            job: Some(job),
            process: process_handle,
            process_id: process.dwProcessId,
            directory,
            title,
            window: None,
        }
    }

    fn wait_window(&mut self) -> HWND {
        struct Search<'a> {
            process: u32,
            title: &'a str,
            found: Option<HWND>,
        }
        unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> BOOL {
            let search = unsafe { &mut *(data.0 as *mut Search<'_>) };
            let mut process = 0;
            unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
            if process == search.process {
                let mut title = [0_u16; 1024];
                let length = unsafe { GetWindowTextW(window, &mut title) }.max(0) as usize;
                if String::from_utf16_lossy(&title[..length]).contains(search.title) {
                    search.found = Some(window);
                    return false.into();
                }
            }
            true.into()
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut search = Search {
                process: self.process_id,
                title: &self.title,
                found: None,
            };
            let _ = unsafe {
                EnumWindows(
                    Some(visit),
                    LPARAM((&mut search as *mut Search<'_>) as isize),
                )
            };
            if let Some(window) = search.found {
                self.window = Some(window);
                return window;
            }
            assert!(
                Instant::now() < deadline,
                "isolated Edge fixture window did not appear"
            );
            assert_eq!(
                unsafe { WaitForSingleObject(self.process.0, 0) },
                WAIT_TIMEOUT,
                "Edge exited before opening the local fixture"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(window) = self.window {
            let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
        let _ = unsafe { WaitForSingleObject(self.process.0, 1500) };
        // The private job contains only this newly created Edge instance and
        // its descendants. Existing browsers/profiles are never enumerated/killed.
        drop(self.job.take());
        let _ = unsafe { WaitForSingleObject(self.process.0, 1500) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while let Err(error) = std::fs::remove_dir_all(&self.directory) {
            if Instant::now() >= deadline {
                eprintln!("fixture profile cleanup failed: {error}");
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from("file:///");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/:.-_~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut url, "%{byte:02X}").unwrap();
        }
    }
    url
}

fn field(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    name: &str,
) -> windows::core::Result<IUIAutomationElement> {
    let condition =
        unsafe { automation.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(name)) }?;
    unsafe { root.FindFirst(TreeScope_Descendants, &condition) }
}

fn read_field(element: &IUIAutomationElement) -> String {
    let value: windows::core::Result<IUIAutomationValuePattern> =
        unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) };
    let text = if let Ok(value) = value {
        unsafe { value.CurrentValue() }.unwrap()
    } else {
        let pattern: IUIAutomationTextPattern =
            unsafe { element.GetCurrentPatternAs(UIA_TextPatternId) }.unwrap();
        let range = unsafe { pattern.DocumentRange() }.unwrap();
        unsafe { range.GetText(4096) }.unwrap()
    };
    String::from_utf16(&text)
        .unwrap()
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

fn focus_target(automation: &IUIAutomation, target: &IUIAutomationElement) {
    unsafe { target.SetFocus() }.unwrap();
    wait_focus(automation, target);
    let pattern: IUIAutomationTextPattern =
        unsafe { target.GetCurrentPatternAs(UIA_TextPatternId) }.unwrap();
    let document = unsafe { pattern.DocumentRange() }.unwrap();
    let prefix = unsafe { document.FindText(&BSTR::from("before "), false, false) }.unwrap();
    unsafe {
        prefix
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                &prefix,
                TextPatternRangeEndpoint_End,
            )
            .unwrap();
        prefix.Select().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let selections = unsafe { pattern.GetSelection() }.unwrap();
        if unsafe { selections.Length() }.unwrap() == 1 {
            let actual = unsafe { selections.GetElement(0) }.unwrap();
            let matches = unsafe {
                actual.CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    &prefix,
                    TextPatternRangeEndpoint_Start,
                )
            }
            .unwrap()
                == 0
                && unsafe {
                    actual.CompareEndpoints(
                        TextPatternRangeEndpoint_End,
                        &prefix,
                        TextPatternRangeEndpoint_End,
                    )
                }
                .unwrap()
                    == 0;
            if matches {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fixture target failed to receive the requested selection"
        );
        thread::sleep(Duration::from_millis(25));
    }
    wait_focus(automation, target);
}

fn wait_focus(automation: &IUIAutomation, target: &IUIAutomationElement) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let focused = unsafe { automation.GetFocusedElement() }.unwrap();
        if unsafe { automation.CompareElements(target, &focused) }
            .unwrap()
            .as_bool()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture target failed to receive focus"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

struct ClipboardText {
    owner: HWND,
    previous: Option<Vec<u16>>,
    sequence: u32,
}

impl ClipboardText {
    fn publish(text: &str) -> Self {
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        open_clipboard(owner);
        let previous = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }
            .ok()
            .and_then(|handle| {
                let global = HGLOBAL(handle.0);
                let size = unsafe { GlobalSize(global) };
                if size > 2_097_152 {
                    return None;
                }
                let data = unsafe { GlobalLock(global) };
                if data.is_null() {
                    return None;
                }
                let units = unsafe { std::slice::from_raw_parts(data.cast::<u16>(), size / 2) };
                let end = units
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(units.len());
                let copy = units[..end].to_vec();
                let _ = unsafe { GlobalUnlock(global) };
                Some(copy)
            });
        unsafe { EmptyClipboard() }.unwrap();
        set_clipboard(&wide(text));
        unsafe { CloseClipboard() }.unwrap();
        Self {
            owner,
            previous,
            sequence: unsafe { GetClipboardSequenceNumber() },
        }
    }
}

impl Drop for ClipboardText {
    fn drop(&mut self) {
        if unsafe { GetClipboardSequenceNumber() } == self.sequence
            && unsafe { OpenClipboard(Some(self.owner)) }.is_ok()
        {
            let _ = unsafe { EmptyClipboard() };
            if let Some(previous) = &self.previous {
                let value: Vec<u16> = previous.iter().copied().chain(Some(0)).collect();
                set_clipboard(&value);
            }
            let _ = unsafe { CloseClipboard() };
        }
        let _ = unsafe { DestroyWindow(self.owner) };
    }
}

fn open_clipboard(owner: HWND) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { OpenClipboard(Some(owner)) }.is_err() {
        assert!(Instant::now() < deadline, "clipboard stayed busy");
        thread::sleep(Duration::from_millis(20));
    }
}

fn set_clipboard(value: &[u16]) {
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(value)) }.unwrap();
    let data = unsafe { GlobalLock(memory) };
    assert!(!data.is_null());
    unsafe { std::ptr::copy_nonoverlapping(value.as_ptr(), data.cast::<u16>(), value.len()) };
    let _ = unsafe { GlobalUnlock(memory) };
    if let Err(error) =
        unsafe { SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0))) }
    {
        let _ = unsafe { GlobalFree(Some(memory)) };
        panic!("fixture clipboard publication failed: {error}");
    }
}

fn paste_once() {
    let input = |key, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [
        input(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        input(VK_V, KEYBD_EVENT_FLAGS(0)),
        input(VK_V, KEYEVENTF_KEYUP),
        input(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        let cleanup = [
            input(VK_V, KEYEVENTF_KEYUP),
            input(VK_CONTROL, KEYEVENTF_KEYUP),
        ];
        let _ = unsafe { SendInput(&cleanup, std::mem::size_of::<INPUT>() as i32) };
    }
    assert_eq!(sent, inputs.len() as u32, "Windows rejected fixture Ctrl+V");
}

#[test]
#[ignore = "requires installed Edge and a Windows desktop; run explicitly in the Windows safety job"]
fn edge_contenteditable_delivery_fixture() {
    let _com = ComApartment::enter();
    let mut browser = Browser::launch();
    let window = browser.wait_window();
    let _ = unsafe { SetForegroundWindow(window) };
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }.unwrap();
    let root = unsafe { automation.ElementFromHandle(window) }.unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let (target_field, other_field) = loop {
        if let (Ok(target), Ok(other)) = (
            field(&automation, &root, TARGET_NAME),
            field(&automation, &root, OTHER_NAME),
        ) {
            break (target, other);
        }
        assert!(
            Instant::now() < deadline,
            "Edge did not expose fixture fields through UIA"
        );
        thread::sleep(Duration::from_millis(100));
    };
    focus_target(&automation, &target_field);
    assert_eq!(read_field(&target_field), "before after");
    assert_eq!(read_field(&other_field), "untouched B");
    let mut target = TargetCapture::begin()
        .resolve()
        .expect("production capture of real contenteditable");
    let active = Arc::new(AtomicBool::new(true));
    if target.requires_foreground_paste() {
        eprintln!("Edge contenteditable route: guarded foreground paste");
        unsafe { other_field.SetFocus() }.unwrap();
        wait_focus(&automation, &other_field);
        assert!(matches!(
            target.prepare_foreground_insert("dictation ", active.clone()),
            Err(TargetError::Changed | TargetError::Unavailable)
        ));
        assert_eq!(read_field(&target_field), "before after");
        assert_eq!(read_field(&other_field), "untouched B");
        drop(target);
        focus_target(&automation, &target_field);
        let mut target = TargetCapture::begin().resolve().unwrap();
        target
            .prepare_foreground_insert("dictation ", active)
            .unwrap();
        let _clipboard = ClipboardText::publish("dictation ");
        assert!(target.foreground_insert_is_ready().unwrap());
        paste_once();
        assert!(target.confirm_foreground_insert().unwrap().verified);
        assert!(target.is_focused_target().unwrap());
    } else {
        eprintln!("Edge contenteditable route: addressed UIA insertion");
        unsafe { other_field.SetFocus() }.unwrap();
        wait_focus(&automation, &other_field);
        let receipt = target
            .insert_if_active("dictation ", active)
            .unwrap_or_else(|error| {
                panic!(
                    "addressed UIA delivery failed: {error:?}; fixture target: {:?}; other field: {:?}",
                    read_field(&target_field),
                    read_field(&other_field)
                )
            });
        assert!(receipt.verified);
        let focused = unsafe { automation.GetFocusedElement() }.unwrap();
        assert!(
            unsafe { automation.CompareElements(&focused, &other_field) }
                .unwrap()
                .as_bool(),
            "background insertion stole focus"
        );
        assert!(!target.is_focused_target().unwrap());
    }
    assert_eq!(read_field(&target_field), "before dictation after");
    assert_eq!(read_field(&other_field), "untouched B");
}
