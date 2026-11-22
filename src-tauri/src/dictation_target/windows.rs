use super::text::{TextSnapshot, MAX_UTF16_UNITS};
use super::write_request::WriteRequest;
use super::{DeliveryReceipt, TargetError};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_TIMEOUT, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
    SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows::Win32::System::Variant::VT_BOOL;
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTextRange, IUIAutomationValuePattern, SetWinEventHook,
    TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId, UIA_IsReadOnlyAttributeId, UIA_TextPatternId, UIA_ValuePatternId,
    UIA_WindowControlTypeId, UnhookWinEvent, HWINEVENTHOOK,
};
use windows::Win32::UI::Controls::{EM_GETSEL, EM_REPLACESEL, EM_SETSEL};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetAncestor, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo,
    GetMessageW, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
    IsWindowUnicode, PeekMessageW, PostThreadMessageW, SendMessageTimeoutW, ES_LOWERCASE,
    ES_NUMBER, ES_PASSWORD, ES_READONLY, ES_UPPERCASE, EVENT_OBJECT_DESTROY, GA_ROOT,
    GUITHREADINFO, GWL_STYLE, MSG, OBJID_WINDOW, PM_REMOVE, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    SMTO_ERRORONEXIT, WINEVENT_OUTOFCONTEXT, WM_APP, WM_GETTEXT, WM_GETTEXTLENGTH,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const MESSAGE_TIMEOUT_MS: u32 = 350;
const READBACK_TIMEOUT: Duration = Duration::from_millis(750);
const WAKE: u32 = WM_APP + 0x5D;

use windows::Win32::UI::WindowsAndMessaging::{EVENT_OBJECT_FOCUS, EVENT_SYSTEM_FOREGROUND};

#[derive(Clone, Copy, PartialEq, Eq)]
struct FocusAnchor {
    foreground: usize,
    focused: usize,
    caret_window: usize,
    caret: [i32; 4],
    process_id: u32,
    thread_id: u32,
}

impl FocusAnchor {
    fn capture() -> Result<Self, TargetError> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0.is_null() {
            return Err(TargetError::Unavailable);
        }
        let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, None) };
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetGUIThreadInfo(foreground_thread, &mut gui) }.map_err(unavailable)?;
        if gui.hwndFocus.0.is_null()
            || unsafe { GetAncestor(gui.hwndFocus, GA_ROOT) } != foreground
            || unsafe { GetForegroundWindow() } != foreground
        {
            return Err(TargetError::Changed);
        }
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(gui.hwndFocus, Some(&mut process_id)) };
        Ok(Self {
            foreground: foreground.0 as usize,
            focused: gui.hwndFocus.0 as usize,
            caret_window: gui.hwndCaret.0 as usize,
            caret: [
                gui.rcCaret.left,
                gui.rcCaret.top,
                gui.rcCaret.right,
                gui.rcCaret.bottom,
            ],
            process_id,
            thread_id,
        })
    }

    fn validate(&self) -> Result<(), TargetError> {
        if Self::capture()? == *self {
            Ok(())
        } else {
            Err(TargetError::Changed)
        }
    }
}

#[derive(Clone, Copy)]
enum CaptureStrategy {
    Automatic,
    #[cfg(test)]
    Foreground,
}

enum Request {
    Insert {
        text: String,
        deadline: Instant,
        writing: Arc<WriteRequest>,
        active: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<DeliveryReceipt, TargetError>>,
    },
    PrepareForeground {
        text: String,
        active: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<(), TargetError>>,
    },
    ForegroundReady(mpsc::Sender<Result<bool, TargetError>>),
    ConfirmForeground(mpsc::Sender<Result<DeliveryReceipt, TargetError>>),
    IsFocused(mpsc::Sender<Result<bool, TargetError>>),
    Close,
}

/// Begin acquisition at the accepted keyboard edge without delaying the audio
/// stop. The ready result can be held across an arbitrarily long transcription.
pub struct TargetCapture {
    pending: Option<PendingCapture>,
    failure: Option<TargetError>,
}

struct PendingCapture {
    sender: Option<mpsc::Sender<Request>>,
    ready: mpsc::Receiver<Result<(u32, bool), TargetError>>,
    deadline: Instant,
    worker_thread: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
}

impl Drop for PendingCapture {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            self.alive.store(false, Ordering::Release);
            let _ = sender.send(Request::Close);
            let thread = self.worker_thread.load(Ordering::Acquire);
            if thread != 0 {
                let _ = unsafe { PostThreadMessageW(thread, WAKE, WPARAM(0), LPARAM(0)) };
            }
        }
    }
}

impl TargetCapture {
    pub fn begin() -> Self {
        Self::begin_with_strategy(CaptureStrategy::Automatic)
    }

    fn begin_with_strategy(strategy: CaptureStrategy) -> Self {
        Self::begin_with_gate(strategy, || {})
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn begin_blocked_for_test(entered: mpsc::Sender<()>, release: mpsc::Receiver<()>) -> Self {
        Self::begin_with_gate(CaptureStrategy::Automatic, move || {
            let _ = entered.send(());
            let _ = release.recv_timeout(REQUEST_TIMEOUT);
        })
    }

    fn begin_with_gate(
        strategy: CaptureStrategy,
        before_acquisition: impl FnOnce() + Send + 'static,
    ) -> Self {
        let anchor = match FocusAnchor::capture() {
            Ok(anchor) => anchor,
            Err(error) => {
                return Self {
                    pending: None,
                    failure: Some(error),
                }
            }
        };
        let (sender, receiver) = mpsc::channel();
        let (ready, result) = mpsc::channel();
        let worker_thread = Arc::new(AtomicU32::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let pending = PendingCapture {
            sender: Some(sender),
            ready: result,
            deadline,
            worker_thread: worker_thread.clone(),
            alive: alive.clone(),
        };
        match thread::Builder::new()
            .name("dictation-target".into())
            .spawn(move || {
                before_acquisition();
                worker(
                    receiver,
                    ready,
                    strategy,
                    anchor,
                    deadline,
                    worker_thread,
                    alive,
                )
            }) {
            Ok(_) => Self {
                pending: Some(pending),
                failure: None,
            },
            Err(_) => Self {
                pending: None,
                failure: Some(TargetError::Unavailable),
            },
        }
    }

    pub fn resolve(mut self) -> Result<TargetSnapshot, TargetError> {
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        let mut pending = self.pending.take().ok_or(TargetError::Unavailable)?;
        // Always accept an already completed acquisition, even when ASR took
        // much longer than the acquisition deadline.
        let ready = match pending.ready.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Disconnected) => Err(TargetError::Unavailable),
            Err(mpsc::TryRecvError::Empty) => pending
                .ready
                .recv_timeout(pending.deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| TargetError::Busy)?,
        };
        let (worker_thread, foreground_only) = ready?;
        Ok(TargetSnapshot {
            sender: pending.sender.take().ok_or(TargetError::Unavailable)?,
            worker_thread,
            used: false,
            foreground_only,
        })
    }
}

/// All COM interfaces and native handles stay on the owning MTA worker. The
/// recording/transcription threads exchange only owned text and channel messages.
#[derive(Debug)]
pub struct TargetSnapshot {
    sender: mpsc::Sender<Request>,
    worker_thread: u32,
    used: bool,
    foreground_only: bool,
}

impl TargetSnapshot {
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn capture() -> Result<Self, TargetError> {
        TargetCapture::begin().resolve()
    }

    #[cfg(test)]
    #[allow(dead_code)] // Also compiled in the main application's test crate.
    pub fn capture_foreground_for_test() -> Result<Self, TargetError> {
        TargetCapture::begin_with_strategy(CaptureStrategy::Foreground).resolve()
    }

    /// Attempt this snapshot exactly once. A timed-out write is uncertain, never
    /// a reason to retry into either this field or the current foreground field.
    #[cfg(test)]
    #[allow(dead_code)] // Convenience for the standalone native fixture.
    pub fn insert(&mut self, text: &str) -> Result<DeliveryReceipt, TargetError> {
        self.insert_if_active(text, Arc::new(AtomicBool::new(true)))
    }

    /// As `insert`, but also rechecks the recording's cancellation token on
    /// the owning worker immediately before any addressed edit.
    pub fn insert_if_active(
        &mut self,
        text: &str,
        active: Arc<AtomicBool>,
    ) -> Result<DeliveryReceipt, TargetError> {
        if self.used {
            return Err(TargetError::AlreadyUsed);
        }
        self.used = true;
        let (reply, result) = mpsc::channel();
        let writing = Arc::new(WriteRequest::default());
        self.send(Request::Insert {
            text: text.to_owned(),
            deadline: Instant::now() + REQUEST_TIMEOUT,
            writing: writing.clone(),
            active,
            reply,
        })?;
        result
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| writing.timed_out())?
    }

    pub fn requires_foreground_paste(&self) -> bool {
        self.foreground_only
    }

    /// Arm one foreground paste only when the exact captured field, value and
    /// selection are still focused. This operation does not itself type/paste.
    pub fn prepare_foreground_insert(
        &mut self,
        text: &str,
        active: Arc<AtomicBool>,
    ) -> Result<(), TargetError> {
        if self.used {
            return Err(TargetError::AlreadyUsed);
        }
        self.used = true;
        let (reply, result) = mpsc::channel();
        self.send(Request::PrepareForeground {
            text: text.to_owned(),
            active,
            reply,
        })?;
        result
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| TargetError::Busy)?
    }

    /// Recheck after clipboard publication and immediately before the caller's
    /// guarded Ctrl+V batch. A changed focus/caret is never followed or restored.
    pub fn foreground_insert_is_ready(&self) -> Result<bool, TargetError> {
        let (reply, result) = mpsc::channel();
        self.send(Request::ForegroundReady(reply))?;
        result
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| TargetError::Busy)?
    }

    pub fn confirm_foreground_insert(&mut self) -> Result<DeliveryReceipt, TargetError> {
        let (reply, result) = mpsc::channel();
        self.send(Request::ConfirmForeground(reply))?;
        result
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| TargetError::Uncertain)?
    }

    /// Recheck the exact focused element, updated value, and caret. This does
    /// not make a subsequent global SendInput atomic: the caller must still
    /// guard its injected batch, and must never submit after a false result.
    pub fn is_focused_target(&self) -> Result<bool, TargetError> {
        let (reply, result) = mpsc::channel();
        self.send(Request::IsFocused(reply))?;
        result
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| TargetError::Busy)?
    }

    fn send(&self, request: Request) -> Result<(), TargetError> {
        self.sender
            .send(request)
            .map_err(|_| TargetError::Unavailable)?;
        // The worker creates its message queue before publishing its thread id.
        unsafe { PostThreadMessageW(self.worker_thread, WAKE, WPARAM(0), LPARAM(0)) }
            .map_err(|_| TargetError::Unavailable)
    }
}

impl Drop for TargetSnapshot {
    fn drop(&mut self) {
        let _ = self.send(Request::Close);
    }
}

fn worker(
    receiver: mpsc::Receiver<Request>,
    ready: mpsc::Sender<Result<(u32, bool), TargetError>>,
    strategy: CaptureStrategy,
    anchor: FocusAnchor,
    deadline: Instant,
    worker_thread: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
) {
    pump_messages();
    worker_thread.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    if !alive.load(Ordering::Acquire) {
        return;
    }
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = ready.send(Err(TargetError::Unavailable));
        return;
    }
    let _com = ComApartment;
    // Force creation of the thread's queue before the first PostThreadMessage.
    pump_messages();
    let mut target = match Target::capture(strategy, &anchor) {
        Ok(target) => target,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if !alive.load(Ordering::Acquire) {
        return;
    }
    if Instant::now() >= deadline {
        let _ = ready.send(Err(TargetError::Busy));
        return;
    }
    if ready
        .send(Ok((
            unsafe { GetCurrentThreadId() },
            matches!(target.writer, Writer::Foreground),
        )))
        .is_err()
    {
        return;
    }
    let mut message = MSG::default();
    // Wait for either a command or an accessibility destruction event. There is
    // no timer polling while the user dictates.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        unsafe { DispatchMessageW(&message) };
        loop {
            match receiver.try_recv() {
                Ok(Request::Insert {
                    text,
                    deadline,
                    writing,
                    active,
                    reply,
                }) => {
                    let _ = reply.send(target.insert(&text, deadline, &writing, &active));
                }
                Ok(Request::IsFocused(reply)) => {
                    let _ = reply.send(target.is_focused());
                }
                Ok(Request::PrepareForeground {
                    text,
                    active,
                    reply,
                }) => {
                    let _ = reply.send(target.prepare_foreground(&text, active));
                }
                Ok(Request::ForegroundReady(reply)) => {
                    let _ = reply.send(target.foreground_ready());
                }
                Ok(Request::ConfirmForeground(reply)) => {
                    let _ = reply.send(target.confirm_foreground());
                }
                Ok(Request::Close) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
    }
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

thread_local! {
    static WATCHED_WINDOWS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    static DESTROYED: Cell<bool> = const { Cell::new(false) };
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    static FOCUS_CHANGED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn on_focus_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if !CAPTURING.with(Cell::get) {
        return;
    }
    let top = WATCHED_WINDOWS.with(|watched| watched.borrow().get(1).copied());
    if event == EVENT_SYSTEM_FOREGROUND {
        if Some(window.0 as isize) != top {
            FOCUS_CHANGED.with(|changed| changed.set(true));
        }
    } else if event == EVENT_OBJECT_FOCUS
        && Some(unsafe { GetAncestor(window, GA_ROOT) }.0 as isize) == top
    {
        // A provider focus event during acquisition is ambiguous even when its
        // native host HWND stayed the same (two fields in one browser window).
        FOCUS_CHANGED.with(|changed| changed.set(true));
    }
}

unsafe extern "system" fn on_window_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    window: HWND,
    object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if object == OBJID_WINDOW.0
        && WATCHED_WINDOWS.with(|watched| watched.borrow().contains(&(window.0 as isize)))
    {
        DESTROYED.with(|destroyed| destroyed.set(true));
    }
}

struct DestructionWatch(Vec<HWINEVENTHOOK>);
impl DestructionWatch {
    fn new(identity: &WindowIdentity) -> Result<Self, TargetError> {
        WATCHED_WINDOWS.with(|watched| {
            *watched.borrow_mut() = vec![identity.hwnd.0 as isize, identity.top.0 as isize];
        });
        DESTROYED.with(|destroyed| destroyed.set(false));
        FOCUS_CHANGED.with(|changed| changed.set(false));
        CAPTURING.with(|capturing| capturing.set(true));
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_DESTROY,
                EVENT_OBJECT_DESTROY,
                None,
                Some(on_window_event),
                identity.process_id,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.0.is_null() {
            Err(TargetError::Unavailable)
        } else {
            let mut hooks = Self(vec![hook]);
            for event in [EVENT_OBJECT_FOCUS, EVENT_SYSTEM_FOREGROUND] {
                let hook = unsafe {
                    SetWinEventHook(
                        event,
                        event,
                        None,
                        Some(on_focus_event),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                if hook.0.is_null() {
                    return Err(TargetError::Unavailable);
                }
                hooks.0.push(hook);
            }
            Ok(hooks)
        }
    }
}
impl Drop for DestructionWatch {
    fn drop(&mut self) {
        for hook in self.0.drain(..) {
            let _ = unsafe { UnhookWinEvent(hook) };
        }
        CAPTURING.with(|capturing| capturing.set(false));
        WATCHED_WINDOWS.with(|watched| watched.borrow_mut().clear());
    }
}

fn pump_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe { DispatchMessageW(&message) };
    }
}

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct WindowIdentity {
    hwnd: HWND,
    top: HWND,
    process_id: u32,
    thread_id: u32,
    process: Process,
    class: String,
    title: Vec<u16>,
}

impl WindowIdentity {
    fn from_anchor(anchor: &FocusAnchor) -> Result<Self, TargetError> {
        anchor.validate()?;
        let top = HWND(anchor.foreground as *mut std::ffi::c_void);
        let hwnd = HWND(anchor.focused as *mut std::ffi::c_void);
        let process_id = anchor.process_id;
        let thread_id = anchor.thread_id;
        let process = Process(
            unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process_id) }.map_err(unavailable)?,
        );
        Ok(Self {
            hwnd,
            top,
            process_id,
            thread_id,
            process,
            class: class_name(hwnd)?,
            title: window_title(top),
        })
    }

    fn validate(&self, check_document: bool) -> Result<(), TargetError> {
        pump_messages();
        if DESTROYED.with(Cell::get)
            || !unsafe { IsWindow(Some(self.hwnd)) }.as_bool()
            || !unsafe { IsWindow(Some(self.top)) }.as_bool()
            || unsafe { WaitForSingleObject(self.process.0, 0) } != WAIT_TIMEOUT
        {
            return Err(TargetError::Unavailable);
        }
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(self.hwnd, Some(&mut process_id)) };
        if process_id != self.process_id
            || thread_id != self.thread_id
            || class_name(self.hwnd)? != self.class
            || unsafe { GetAncestor(self.hwnd, GA_ROOT) } != self.top
        {
            return Err(TargetError::Unavailable);
        }
        if check_document && window_title(self.top) != self.title {
            return Err(TargetError::Changed);
        }
        Ok(())
    }
}

fn class_name(hwnd: HWND) -> Result<String, TargetError> {
    let mut name = [0_u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut name) };
    if length <= 0 {
        return Err(TargetError::Unavailable);
    }
    String::from_utf16(&name[..length as usize]).map_err(|_| TargetError::Unsupported)
}

fn window_title(hwnd: HWND) -> Vec<u16> {
    let mut title = vec![0_u16; 8192];
    let length = unsafe { GetWindowTextW(hwnd, &mut title) };
    title.truncate(length.max(0) as usize);
    title
}

struct ContextElement {
    element: IUIAutomationElement,
    runtime_id: Vec<i32>,
    name: Vec<u16>,
}

impl ContextElement {
    fn capture(element: IUIAutomationElement) -> Result<Self, TargetError> {
        let name = unsafe { element.CurrentName() }.map_err(unavailable)?;
        Ok(Self {
            runtime_id: runtime_id(&element)?,
            name: name.to_vec(),
            element,
        })
    }

    fn validate(&self) -> Result<(), TargetError> {
        if runtime_id(&self.element)? != self.runtime_id {
            return Err(TargetError::Unavailable);
        }
        if unsafe { self.element.CurrentName() }
            .map_err(unavailable)?
            .to_vec()
            != self.name
        {
            return Err(TargetError::Changed);
        }
        Ok(())
    }
}

enum Writer {
    Native,
    Value(IUIAutomationValuePattern),
    Foreground,
}

struct PendingForeground {
    expected: Vec<u16>,
    caret: usize,
    active: Arc<AtomicBool>,
}

struct Target {
    automation: IUIAutomation,
    element: IUIAutomationElement,
    element_id: Vec<i32>,
    identity: WindowIdentity,
    _watch: DestructionWatch,
    context: Vec<ContextElement>,
    writer: Writer,
    text: TextSnapshot,
    inserted: bool,
    caret_verified: bool,
    pending_foreground: Option<PendingForeground>,
    submit_allowed: bool,
}

impl Target {
    fn capture(strategy: CaptureStrategy, anchor: &FocusAnchor) -> Result<Self, TargetError> {
        let identity = WindowIdentity::from_anchor(anchor)?;
        let watch = DestructionWatch::new(&identity)?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }
                .map_err(unavailable)?;
        let timeouts: IUIAutomation2 = automation.cast().map_err(unavailable)?;
        unsafe { timeouts.SetConnectionTimeout(MESSAGE_TIMEOUT_MS) }.map_err(unavailable)?;
        unsafe { timeouts.SetTransactionTimeout(MESSAGE_TIMEOUT_MS) }.map_err(unavailable)?;
        let element = unsafe { automation.GetFocusedElement() }.map_err(unavailable)?;
        validate_editable(&element)?;
        if unsafe { element.CurrentProcessId() }.map_err(unavailable)? as u32 != identity.process_id
        {
            return Err(TargetError::Changed);
        }
        let element_id = runtime_id(&element)?;
        let native = matches!(strategy, CaptureStrategy::Automatic)
            && (identity.class.eq_ignore_ascii_case("edit")
                || identity.class.to_ascii_lowercase().starts_with("richedit"));
        let (writer, text) = if native {
            if unsafe { element.CurrentNativeWindowHandle() }.map_err(unavailable)? != identity.hwnd
                || !unsafe { IsWindowUnicode(identity.hwnd) }.as_bool()
            {
                return Err(TargetError::Unsupported);
            }
            native_editable(identity.hwnd)?;
            let value = native_value(identity.hwnd)?;
            let selection = native_selection(identity.hwnd)?;
            // RichEdit and Win32 Edit use different newline conventions. When
            // RichEdit exposes CRLF, require its UIA offsets to agree exactly.
            if !identity.class.eq_ignore_ascii_case("edit")
                && value.windows(2).any(|pair| pair == [13, 10])
                && uia_selection(&element, &value)? != selection
            {
                return Err(TargetError::Unsupported);
            }
            (Writer::Native, TextSnapshot::new(value, selection)?)
        } else if let Some(value_pattern) = matches!(strategy, CaptureStrategy::Automatic)
            .then(|| unsafe {
                element
                    .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .ok()
            })
            .flatten()
        {
            if unsafe { value_pattern.CurrentIsReadOnly() }
                .map_err(unavailable)?
                .as_bool()
            {
                return Err(TargetError::Protected);
            }
            let value = unsafe { value_pattern.CurrentValue() }
                .map_err(unavailable)?
                .to_vec();
            let selection = uia_selection(&element, &value)?;
            (
                Writer::Value(value_pattern),
                TextSnapshot::new(value, selection)?,
            )
        } else {
            let pattern = foreground_pattern(&element)?;
            let document = unsafe { pattern.DocumentRange() }.map_err(unavailable)?;
            let value = range_text(&document)?;
            let selection = uia_selection(&element, &value)?;
            (Writer::Foreground, TextSnapshot::new(value, selection)?)
        };
        let walker = unsafe { automation.RawViewWalker() }.map_err(unavailable)?;
        let mut context = Vec::new();
        let mut ancestor = element.clone();
        for _ in 0..16 {
            ancestor = unsafe { walker.GetParentElement(&ancestor) }.map_err(unavailable)?;
            let is_window = unsafe { ancestor.CurrentControlType() }.map_err(unavailable)?
                == UIA_WindowControlTypeId;
            context.push(ContextElement::capture(ancestor.clone())?);
            if is_window {
                break;
            }
        }
        let target = Self {
            automation,
            element,
            element_id,
            identity,
            _watch: watch,
            context,
            writer,
            text,
            inserted: false,
            caret_verified: false,
            pending_foreground: None,
            submit_allowed: false,
        };
        target.validate(true)?;
        // Capture only if all reads referred to the same focused field/value.
        if target.current_value()? != target.text.value
            || target.current_selection(&target.text.value)? != target.text.selection
            || !target.focus_matches()?
        {
            return Err(TargetError::Changed);
        }
        anchor.validate()?;
        pump_messages();
        if FOCUS_CHANGED.with(Cell::get) {
            return Err(TargetError::Changed);
        }
        CAPTURING.with(|capturing| capturing.set(false));
        Ok(target)
    }

    fn validate(&self, check_document: bool) -> Result<(), TargetError> {
        self.identity.validate(check_document)?;
        validate_editable(&self.element)?;
        if runtime_id(&self.element)? != self.element_id {
            return Err(TargetError::Unavailable);
        }
        if check_document {
            for context in &self.context {
                context.validate()?;
            }
        }
        match &self.writer {
            Writer::Native => native_editable(self.identity.hwnd),
            Writer::Value(value) => {
                if unsafe { value.CurrentIsReadOnly() }
                    .map_err(unavailable)?
                    .as_bool()
                {
                    Err(TargetError::Protected)
                } else {
                    Ok(())
                }
            }
            Writer::Foreground => foreground_pattern(&self.element).map(|_| ()),
        }
    }

    fn current_value(&self) -> Result<Vec<u16>, TargetError> {
        match &self.writer {
            Writer::Native => native_value(self.identity.hwnd),
            Writer::Value(value) => Ok(unsafe { value.CurrentValue() }
                .map_err(unavailable)?
                .to_vec()),
            Writer::Foreground => {
                let pattern = text_pattern(&self.element)?;
                range_text(&unsafe { pattern.DocumentRange() }.map_err(unavailable)?)
            }
        }
    }

    fn insert(
        &mut self,
        text: &str,
        deadline: Instant,
        writing: &WriteRequest,
        active: &AtomicBool,
    ) -> Result<DeliveryReceipt, TargetError> {
        if !active.load(Ordering::Acquire) {
            return Err(TargetError::Unavailable);
        }
        if self.inserted {
            return Err(TargetError::AlreadyUsed);
        }
        if matches!(self.writer, Writer::Foreground) {
            return Err(TargetError::Unsupported);
        }
        self.validate(true)?;
        let expected = self.text.replacement(&self.current_value()?, text)?;
        let caret = self.text.selection.start + text.encode_utf16().count();
        self.validate(true)?;
        self.submit_allowed = self.focus_matches().unwrap_or(false);
        if Instant::now() >= deadline || !active.load(Ordering::Acquire) {
            return Err(TargetError::Busy);
        }
        match &self.writer {
            Writer::Native => {
                let replacement: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                if Instant::now() >= deadline || !active.load(Ordering::Acquire) {
                    return Err(TargetError::Busy);
                }
                // EM_SETSEL is already a mutation. Claim before it, so an
                // aborted caller cannot be followed by a late caret change.
                writing.claim_write()?;
                message(
                    self.identity.hwnd,
                    EM_SETSEL,
                    self.text.selection.start,
                    self.text.selection.end as isize,
                )?;
                if native_selection(self.identity.hwnd)? != self.text.selection
                    || self.current_value()? != self.text.value
                {
                    return Err(TargetError::Changed);
                }
                self.identity.validate(true)?;
                if Instant::now() >= deadline || !active.load(Ordering::Acquire) {
                    return Err(TargetError::Busy);
                }
                self.inserted = true;
                message(
                    self.identity.hwnd,
                    EM_REPLACESEL,
                    1,
                    replacement.as_ptr() as isize,
                )
                .map_err(|_| TargetError::Uncertain)?;
            }
            Writer::Value(value) => {
                let replacement = BSTR::from_wide(&expected);
                // Ancestor/focus validation can involve provider RPCs. Refresh
                // the source value after them, immediately before the write.
                if unsafe { value.CurrentValue() }
                    .map_err(unavailable)?
                    .to_vec()
                    != self.text.value
                {
                    return Err(TargetError::Changed);
                }
                if !active.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err(TargetError::Busy);
                }
                writing.claim_write()?;
                self.inserted = true;
                unsafe { value.SetValue(&replacement) }.map_err(|_| TargetError::Uncertain)?;
            }
            Writer::Foreground => return Err(TargetError::Unsupported),
        }
        let readback_deadline = deadline.min(Instant::now() + READBACK_TIMEOUT);
        let mut backoff = Duration::from_millis(15);
        loop {
            self.validate(false).map_err(|_| TargetError::Uncertain)?;
            let current = self.current_value().map_err(|_| TargetError::Uncertain)?;
            if current == expected {
                break;
            }
            if !matches!(self.writer, Writer::Value(_))
                || current != self.text.value
                || Instant::now() >= readback_deadline
                || !active.load(Ordering::Acquire)
            {
                return Err(TargetError::Uncertain);
            }
            // A UIA SetValue acknowledgement can precede the renderer's update
            // (for example in Chromium). Retry only reads of the addressed
            // element while its original value remains intact, never SetValue.
            thread::sleep(backoff.min(readback_deadline.saturating_duration_since(Instant::now())));
            backoff = (backoff * 2).min(Duration::from_millis(120));
        }
        // A caret restoration failure must not trigger another insertion. The
        // text is already confirmed; it only disables subsequent auto-submit.
        self.caret_verified = match &self.writer {
            Writer::Native => native_selection(self.identity.hwnd).ok() == Some(caret..caret),
            Writer::Value(_) => {
                self.submit_allowed
                    && self.focus_matches().unwrap_or(false)
                    && restore_uia_caret(&self.element, &expected, caret).is_ok()
            }
            Writer::Foreground => false,
        };
        self.text = TextSnapshot::new(expected, caret..caret)?;
        Ok(DeliveryReceipt { verified: true })
    }

    fn current_selection(&self, value: &[u16]) -> Result<std::ops::Range<usize>, TargetError> {
        match &self.writer {
            Writer::Native => native_selection(self.identity.hwnd),
            Writer::Value(_) | Writer::Foreground => uia_selection(&self.element, value),
        }
    }

    fn prepare_foreground(
        &mut self,
        text: &str,
        active: Arc<AtomicBool>,
    ) -> Result<(), TargetError> {
        if self.inserted {
            return Err(TargetError::AlreadyUsed);
        }
        if !active.load(Ordering::Acquire) {
            return Err(TargetError::Unavailable);
        }
        self.validate(true)?;
        if self.current_selection(&self.text.value)? != self.text.selection {
            return Err(TargetError::Changed);
        }
        let expected = self.text.replacement(&self.current_value()?, text)?;
        let caret = self.text.selection.start + text.encode_utf16().count();
        if !active.load(Ordering::Acquire) || !self.focus_matches()? {
            return Err(TargetError::Unavailable);
        }
        self.pending_foreground = Some(PendingForeground {
            expected,
            caret,
            active,
        });
        self.inserted = true;
        self.submit_allowed = true;
        Ok(())
    }

    fn foreground_ready(&self) -> Result<bool, TargetError> {
        let Some(pending) = &self.pending_foreground else {
            return Ok(false);
        };
        if !pending.active.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.validate(true)?;
        Ok(self.current_value()? == self.text.value
            && self.current_selection(&self.text.value)? == self.text.selection
            && self.focus_matches()?
            && pending.active.load(Ordering::Acquire))
    }

    fn confirm_foreground(&mut self) -> Result<DeliveryReceipt, TargetError> {
        let pending = self
            .pending_foreground
            .take()
            .ok_or(TargetError::AlreadyUsed)?;
        let deadline = Instant::now() + READBACK_TIMEOUT;
        let mut backoff = Duration::from_millis(15);
        loop {
            self.validate(false).map_err(|_| TargetError::Uncertain)?;
            let current = self.current_value().map_err(|_| TargetError::Uncertain)?;
            if current == pending.expected {
                break;
            }
            if current != self.text.value
                || Instant::now() >= deadline
                || !pending.active.load(Ordering::Acquire)
                || !self.focus_matches().unwrap_or(false)
            {
                return Err(TargetError::Uncertain);
            }
            // Only readback is retried; the caller's paste is issued once.
            thread::sleep(backoff.min(deadline.saturating_duration_since(Instant::now())));
            backoff = (backoff * 2).min(Duration::from_millis(120));
        }
        self.caret_verified =
            self.current_selection(&pending.expected).ok() == Some(pending.caret..pending.caret);
        self.text = TextSnapshot::new(pending.expected, pending.caret..pending.caret)?;
        Ok(DeliveryReceipt { verified: true })
    }

    fn native_focus_matches(&self) -> Result<bool, TargetError> {
        if unsafe { GetForegroundWindow() } != self.identity.top {
            return Ok(false);
        }
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetGUIThreadInfo(self.identity.thread_id, &mut gui) }.map_err(unavailable)?;
        Ok(gui.hwndFocus == self.identity.hwnd)
    }

    fn focus_matches(&self) -> Result<bool, TargetError> {
        if !self.native_focus_matches()? {
            return Ok(false);
        }
        let focused = unsafe { self.automation.GetFocusedElement() }.map_err(unavailable)?;
        Ok(runtime_id(&focused)? == self.element_id
            && unsafe { self.automation.CompareElements(&self.element, &focused) }
                .map_err(unavailable)?
                .as_bool()
            && self.native_focus_matches()?)
    }

    fn is_focused(&self) -> Result<bool, TargetError> {
        if !self.inserted || !self.caret_verified || !self.submit_allowed {
            return Ok(false);
        }
        self.validate(false)?;
        if self.current_value()? != self.text.value {
            return Ok(false);
        }
        let selection = self.current_selection(&self.text.value)?;
        Ok(selection == self.text.selection && self.focus_matches()?)
    }
}

fn foreground_pattern(
    element: &IUIAutomationElement,
) -> Result<IUIAutomationTextPattern, TargetError> {
    let control = unsafe { element.CurrentControlType() }.map_err(unavailable)?;
    if (control != UIA_EditControlTypeId && control != UIA_DocumentControlTypeId)
        || !unsafe { element.CurrentIsKeyboardFocusable() }
            .map_err(unavailable)?
            .as_bool()
    {
        return Err(TargetError::Unsupported);
    }
    let pattern = text_pattern(element)?;
    let document = unsafe { pattern.DocumentRange() }.map_err(unavailable)?;
    let read_only =
        unsafe { document.GetAttributeValue(UIA_IsReadOnlyAttributeId) }.map_err(unavailable)?;
    // Mixed/unsupported attributes are not evidence of an editable field.
    if read_only.vt() != VT_BOOL {
        return Err(TargetError::Unsupported);
    }
    if bool::try_from(&read_only).map_err(unavailable)? {
        return Err(TargetError::Protected);
    }
    Ok(pattern)
}

fn validate_editable(element: &IUIAutomationElement) -> Result<(), TargetError> {
    if unsafe { element.CurrentIsPassword() }
        .map_err(unavailable)?
        .as_bool()
        || !unsafe { element.CurrentIsEnabled() }
            .map_err(unavailable)?
            .as_bool()
    {
        return Err(TargetError::Protected);
    }
    Ok(())
}

fn native_editable(hwnd: HWND) -> Result<(), TargetError> {
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
    if style & (ES_PASSWORD | ES_READONLY) != 0 {
        Err(TargetError::Protected)
    } else if style & (ES_NUMBER | ES_UPPERCASE | ES_LOWERCASE) != 0 {
        // These controls can silently transform or discard transcription text.
        Err(TargetError::Unsupported)
    } else {
        Ok(())
    }
}

fn message(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> Result<usize, TargetError> {
    let mut result = 0;
    let delivered = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_BLOCK | SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            MESSAGE_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    if delivered.0 == 0 {
        Err(TargetError::Busy)
    } else {
        Ok(result)
    }
}

fn native_value(hwnd: HWND) -> Result<Vec<u16>, TargetError> {
    let length = message(hwnd, WM_GETTEXTLENGTH, 0, 0)?;
    if length > MAX_UTF16_UNITS {
        return Err(TargetError::Unsupported);
    }
    let mut value = vec![0_u16; length + 1];
    let copied = message(hwnd, WM_GETTEXT, value.len(), value.as_mut_ptr() as isize)?;
    if copied > length || message(hwnd, WM_GETTEXTLENGTH, 0, 0)? != copied {
        return Err(TargetError::Changed);
    }
    value.truncate(copied);
    Ok(value)
}

fn native_selection(hwnd: HWND) -> Result<std::ops::Range<usize>, TargetError> {
    let mut start = 0_u32;
    let mut end = 0_u32;
    message(
        hwnd,
        EM_GETSEL,
        (&mut start as *mut u32) as usize,
        (&mut end as *mut u32) as isize,
    )?;
    Ok(start as usize..end as usize)
}

fn text_pattern(element: &IUIAutomationElement) -> Result<IUIAutomationTextPattern, TargetError> {
    unsafe { element.GetCurrentPatternAs(UIA_TextPatternId) }.map_err(|_| TargetError::Unsupported)
}

fn range_text(range: &IUIAutomationTextRange) -> Result<Vec<u16>, TargetError> {
    let value = unsafe { range.GetText((MAX_UTF16_UNITS + 1) as i32) }.map_err(unavailable)?;
    if value.len() > MAX_UTF16_UNITS {
        return Err(TargetError::Unsupported);
    }
    Ok(value.to_vec())
}

fn uia_selection(
    element: &IUIAutomationElement,
    expected: &[u16],
) -> Result<std::ops::Range<usize>, TargetError> {
    let pattern = text_pattern(element)?;
    let document = unsafe { pattern.DocumentRange() }.map_err(unavailable)?;
    if range_text(&document)? != expected {
        return Err(TargetError::Unsupported);
    }
    let selections = unsafe { pattern.GetSelection() }.map_err(unavailable)?;
    if unsafe { selections.Length() }.map_err(unavailable)? != 1 {
        return Err(TargetError::Unsupported);
    }
    let selected = unsafe { selections.GetElement(0) }.map_err(unavailable)?;
    let before = unsafe { document.Clone() }.map_err(unavailable)?;
    unsafe {
        before.MoveEndpointByRange(
            TextPatternRangeEndpoint_End,
            &selected,
            TextPatternRangeEndpoint_Start,
        )
    }
    .map_err(unavailable)?;
    let start = range_text(&before)?.len();
    unsafe {
        before.MoveEndpointByRange(
            TextPatternRangeEndpoint_End,
            &selected,
            TextPatternRangeEndpoint_End,
        )
    }
    .map_err(unavailable)?;
    let end = range_text(&before)?.len();
    if start > end || end > expected.len() || range_text(&selected)? != expected[start..end] {
        return Err(TargetError::Changed);
    }
    Ok(start..end)
}

fn restore_uia_caret(
    element: &IUIAutomationElement,
    value: &[u16],
    offset: usize,
) -> Result<(), TargetError> {
    let pattern = text_pattern(element)?;
    let document = unsafe { pattern.DocumentRange() }.map_err(unavailable)?;
    if range_text(&document)? != value {
        return Err(TargetError::Changed);
    }
    let caret = unsafe { document.Clone() }.map_err(unavailable)?;
    if offset == 0 {
        unsafe {
            caret.MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                &document,
                TextPatternRangeEndpoint_Start,
            )
        }
        .map_err(unavailable)?;
    } else {
        let prefix = String::from_utf16(&value[..offset]).map_err(|_| TargetError::InvalidText)?;
        let range =
            unsafe { document.FindText(&BSTR::from(prefix), false, false) }.map_err(unavailable)?;
        if unsafe {
            range.CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &document,
                TextPatternRangeEndpoint_Start,
            )
        }
        .map_err(unavailable)?
            != 0
            || range_text(&range)? != value[..offset]
        {
            return Err(TargetError::Changed);
        }
        unsafe {
            caret.MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                &range,
                TextPatternRangeEndpoint_End,
            )
        }
        .map_err(unavailable)?;
    }
    unsafe {
        caret.MoveEndpointByRange(
            TextPatternRangeEndpoint_Start,
            &caret,
            TextPatternRangeEndpoint_End,
        )
    }
    .map_err(unavailable)?;
    unsafe { caret.Select() }.map_err(unavailable)?;
    if uia_selection(element, value)? != (offset..offset) {
        return Err(TargetError::Changed);
    }
    Ok(())
}

fn runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, TargetError> {
    struct Array(*mut SAFEARRAY);
    impl Drop for Array {
        fn drop(&mut self) {
            let _ = unsafe { SafeArrayDestroy(self.0) };
        }
    }
    let array = Array(unsafe { element.GetRuntimeId() }.map_err(unavailable)?);
    if array.0.is_null()
        || unsafe { SafeArrayGetDim(array.0) } != 1
        || unsafe { SafeArrayGetElemsize(array.0) } != 4
    {
        return Err(TargetError::Unsupported);
    }
    let lower = unsafe { SafeArrayGetLBound(array.0, 1) }.map_err(unavailable)?;
    let upper = unsafe { SafeArrayGetUBound(array.0, 1) }.map_err(unavailable)?;
    if upper < lower || i64::from(upper) - i64::from(lower) > 64 {
        return Err(TargetError::Unsupported);
    }
    let mut result = Vec::new();
    for index in lower..=upper {
        let mut value = 0_i32;
        unsafe { SafeArrayGetElement(array.0, &index, (&mut value as *mut i32).cast()) }
            .map_err(unavailable)?;
        result.push(value);
    }
    Ok(result)
}

fn unavailable(_error: windows::core::Error) -> TargetError {
    // Provider errors may include application/user text. Never expose or log
    // the raw error while returning a stable reason for the fallback notice.
    TargetError::Unavailable
}
