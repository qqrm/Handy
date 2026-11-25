//! Production coordinator/action/manager bodies are inserted by
//! coordinator_regressions.py. Only Tauri and external audio/model/UI services
//! are fakes. Gates force capture and post-capture cleanup to remain in flight.
#![allow(dead_code, unused_imports, unused_variables)]

extern crate self as log;
extern crate self as tauri;

#[macro_export]
macro_rules! debug { ($($a:tt)*) => {{ let _ = format_args!($($a)*); }} }
#[macro_export]
macro_rules! info { ($($a:tt)*) => {{ let _ = format_args!($($a)*); }} }
#[macro_export]
macro_rules! warn { ($($a:tt)*) => {{ let _ = format_args!($($a)*); }} }
#[macro_export]
macro_rules! error { ($($a:tt)*) => {{ let _ = format_args!($($a)*); }} }

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub struct AppHandle(Arc<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>);

pub struct State<T>(Arc<T>);
impl<T> Deref for State<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

pub trait Manager {
    fn try_state<T: Any + Send + Sync>(&self) -> Option<State<T>>;
    fn state<T: Any + Send + Sync>(&self) -> State<T> {
        self.try_state().expect("fixture state is registered")
    }
}

impl Manager for AppHandle {
    fn try_state<T: Any + Send + Sync>(&self) -> Option<State<T>> {
        let value = self.0.lock().unwrap().get(&TypeId::of::<T>())?.clone();
        value.downcast::<T>().ok().map(State)
    }
}

impl AppHandle {
    fn manage<T: Any + Send + Sync>(&self, value: T) {
        self.0
            .lock()
            .unwrap()
            .insert(TypeId::of::<T>(), Arc::new(value));
    }
    fn emit<T>(&self, _: &str, _: T) -> Result<(), ()> {
        Ok(())
    }
}

struct Gate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

struct GateControl {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

fn gate() -> (Gate, GateControl) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    (
        Gate {
            entered: entered_tx,
            release: release_rx,
        },
        GateControl {
            entered: entered_rx,
            release: release_tx,
        },
    )
}

impl Gate {
    fn block(self) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(TIMEOUT).unwrap();
    }
}

impl GateControl {
    fn wait_entered(&self) {
        self.entered.recv_timeout(TIMEOUT).unwrap();
    }
    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

#[derive(Default)]
struct ExternalEffects {
    capture_gate: Mutex<Option<Gate>>,
    cleanup_gate: Mutex<Option<Gate>>,
    starts: AtomicUsize,
    capture_stops: AtomicUsize,
    cancellations: AtomicUsize,
    stream_cancels: AtomicUsize,
    overlay_visible: AtomicBool,
    tray_recording: AtomicBool,
}

mod settings {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ShortcutActivation {
        PushToTalk,
        HoldOrToggle,
        Toggle,
    }
    pub enum OverlayStyle {
        Live,
        Minimal,
        None,
    }
    pub struct AppSettings {
        pub selected_model: String,
        pub always_on_microphone: bool,
        pub vad_enabled: bool,
        pub overlay_style: OverlayStyle,
        pub mute_while_recording: bool,
        pub pause_media_while_recording: bool,
        pub extra_recording_buffer_ms: u64,
        pub lazy_stream_close: bool,
    }
    pub fn get_settings(_: &crate::AppHandle) -> AppSettings {
        AppSettings {
            selected_model: "test-model".into(),
            always_on_microphone: true,
            vad_enabled: false,
            overlay_style: OverlayStyle::Minimal,
            mute_while_recording: true,
            pause_media_while_recording: true,
            extra_recording_buffer_ms: 0,
            lazy_stream_close: false,
        }
    }
}
use settings::{get_settings, OverlayStyle};

enum VadPolicy {
    Disabled,
    Streaming,
    Offline,
}
enum SoundType {
    Start,
}
fn play_feedback_sound_blocking(_: &AppHandle, _: SoundType) {}
fn is_microphone_access_denied(_: &str) -> bool {
    false
}
fn is_no_input_device_error(_: &str) -> bool {
    false
}

mod memory {
    pub fn trim_freed_memory() {}
}
mod shortcut {
    use super::*;
    pub fn register_cancel_shortcut(_: &AppHandle) {}
    pub fn unregister_cancel_shortcut(app: &AppHandle) {
        app.state::<Arc<ExternalEffects>>()
            .cancellations
            .fetch_add(1, Ordering::SeqCst);
    }
}
mod tray {
    use super::*;
    pub enum TrayIconState {
        Idle,
        Recording,
    }
    pub fn set_tray_state(app: &AppHandle, state: TrayIconState) {
        app.state::<Arc<ExternalEffects>>()
            .tray_recording
            .store(matches!(state, TrayIconState::Recording), Ordering::SeqCst);
    }
}
use tray::{set_tray_state, TrayIconState};

mod managers {
    use super::*;
    pub mod audio {
        use super::*;
        const WHISPER_SAMPLE_RATE: usize = 16000;
        /* PRODUCTION_RECORDING_TYPES */

        struct OutputOptions {
            mute: bool,
            pause: bool,
        }
        struct OutputBackend;
        impl OutputBackend {
            fn begin(&self, _: u64, _: OutputOptions) {}
            fn stop_requested(&self, _: u64) {}
            fn finish_after_capture<T>(&self, _: u64, capture: impl FnOnce() -> T) -> Option<T> {
                Some(capture())
            }
        }

        struct Recorder(Arc<ExternalEffects>);
        impl Recorder {
            fn start(&self, _: VadPolicy) -> Result<mpsc::Receiver<()>, String> {
                self.0.starts.fetch_add(1, Ordering::SeqCst);
                // No hardware samples are supplied. The real readiness worker
                // exits without a cue when this sender is dropped.
                let (_, rx) = mpsc::channel();
                Ok(rx)
            }
            fn stop(&self) -> Result<Vec<f32>, String> {
                self.0.capture_stops.fetch_add(1, Ordering::SeqCst);
                let gate = self.0.capture_gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.block();
                }
                Ok(Vec::new())
            }
        }

        pub struct AudioRecordingManager {
            state: Mutex<RecordingState>,
            mode: Mutex<MicrophoneMode>,
            app_handle: AppHandle,
            recorder: Mutex<Option<Recorder>>,
            is_recording: Mutex<bool>,
            recording_output: OutputBackend,
            close_generation: AtomicU64,
            cancel_generation: AtomicU64,
            recording_active: AtomicBool,
            capture_generation: AtomicU64,
        }

        impl AudioRecordingManager {
            pub fn new_fixture(app: &AppHandle, effects: Arc<ExternalEffects>) -> Self {
                Self {
                    state: Mutex::new(RecordingState::Idle),
                    mode: Mutex::new(MicrophoneMode::AlwaysOn),
                    app_handle: app.clone(),
                    recorder: Mutex::new(Some(Recorder(effects))),
                    is_recording: Mutex::new(false),
                    recording_output: OutputBackend,
                    close_generation: AtomicU64::new(0),
                    cancel_generation: AtomicU64::new(0),
                    recording_active: AtomicBool::new(false),
                    capture_generation: AtomicU64::new(0),
                }
            }
            pub fn preload_vad(&self) -> Result<(), String> {
                Ok(())
            }
            fn start_microphone_stream(&self) -> Result<(), String> {
                Ok(())
            }
            fn schedule_lazy_close(&self) {
                unreachable!("fixture is always-on")
            }
            fn stop_microphone_stream(&self) {
                unreachable!("fixture is always-on")
            }
            pub fn apply_recording_output(&self, _: u64) {}
            /* PRODUCTION_AUDIO_METHODS */
        }
    }

    pub mod model {
        pub struct ModelManager;
        pub struct ModelInfo {
            pub supports_streaming: bool,
        }
        impl ModelManager {
            pub fn get_model_path(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            pub fn get_model_info(&self, _: &str) -> Option<ModelInfo> {
                Some(ModelInfo {
                    supports_streaming: false,
                })
            }
        }
    }

    pub mod transcription {
        use super::*;
        pub struct TranscriptionManager(pub Arc<ExternalEffects>);
        impl TranscriptionManager {
            pub fn initiate_model_load(&self) {}
            pub fn is_model_loaded(&self) -> bool {
                true
            }
            pub fn start_stream(&self) {}
            pub fn maybe_unload_immediately(&self, _: &str) {}
            pub fn cancel_stream(&self) {
                self.0.stream_cancels.fetch_add(1, Ordering::SeqCst);
                let gate = self.0.cleanup_gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.block();
                }
            }
        }
    }
}
use managers::audio::AudioRecordingManager;
use managers::model::ModelManager;
use managers::transcription::TranscriptionManager;

mod utils {
    use super::*;
    /* PRODUCTION_CANCEL_FUNCTIONS */
    pub fn show_streaming_overlay(app: &AppHandle) {
        show_recording_overlay(app);
    }
    pub fn show_recording_overlay(app: &AppHandle) {
        app.state::<Arc<ExternalEffects>>()
            .overlay_visible
            .store(true, Ordering::SeqCst);
    }
    pub fn hide_recording_overlay(app: &AppHandle) {
        app.state::<Arc<ExternalEffects>>()
            .overlay_visible
            .store(false, Ordering::SeqCst);
    }
    pub fn emit_recording_ready(_: &AppHandle) {}
}
use utils::show_recording_overlay;

mod actions {
    use super::*;
    struct RecordingErrorEvent {
        error_type: String,
        detail: Option<String>,
    }
    /* PRODUCTION_FINISH_GUARD */
    /* PRODUCTION_ACTION_TRAIT */
    pub struct TranscribeAction {
        pub post_process: bool,
    }
    impl ShortcutAction for TranscribeAction {
        /* PRODUCTION_ACTION_START */
        /* PRODUCTION_STOP_FUNCTION */
    }
    pub static ACTION_MAP: LazyLock<HashMap<String, Arc<dyn ShortcutAction>>> =
        LazyLock::new(|| {
            HashMap::from([(
                "transcribe".into(),
                Arc::new(TranscribeAction {
                    post_process: false,
                }) as Arc<dyn ShortcutAction>,
            )])
        });
}

mod transcription_coordinator {
    /* PRODUCTION_COORDINATOR */

    #[cfg(test)]
    mod runtime_tests {
        use super::*;
        use crate::actions::ShortcutAction;
        use crate::{
            gate, AppHandle, AudioRecordingManager, ExternalEffects, Manager, ModelManager,
            TranscriptionManager, TIMEOUT,
        };
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        fn fixture() -> (
            AppHandle,
            Arc<ExternalEffects>,
            Sender<Command>,
            mpsc::Receiver<Command>,
        ) {
            let app = AppHandle::default();
            let effects = Arc::new(ExternalEffects::default());
            app.manage(Arc::clone(&effects));
            app.manage(Arc::new(AudioRecordingManager::new_fixture(
                &app,
                Arc::clone(&effects),
            )));
            app.manage(Arc::new(TranscriptionManager(Arc::clone(&effects))));
            app.manage(Arc::new(ModelManager));
            let (tx, rx) = mpsc::channel();
            app.manage(TranscriptionCoordinator { tx: tx.clone() });
            (app, effects, tx, rx)
        }

        fn input() -> Command {
            Command::Input(InputEvent {
                binding_id: "transcribe".into(),
                hotkey_string: "fixture".into(),
                is_pressed: true,
                mode: ShortcutActivation::Toggle,
                hold_threshold: Duration::ZERO,
                external: true,
            })
        }

        fn deliver(
            app: &AppHandle,
            state: &mut CoordinatorState,
            tx: &Sender<Command>,
            rx: &mpsc::Receiver<Command>,
        ) {
            let command = rx.recv_timeout(TIMEOUT).unwrap();
            dispatch_command(app, state, command, tx);
        }

        #[test]
        fn no_token_stop_waits_for_real_capture_and_full_cancel_cleanup() {
            let (app, effects, tx, rx) = fixture();
            let mut state = CoordinatorState::new();
            dispatch_command(&app, &mut state, input(), &tx);
            assert_eq!(effects.starts.load(Ordering::SeqCst), 1);

            // Recreate the critic's gap between the coordinator deciding Stop
            // and the action obtaining the capture token. The new cancellation
            // fence must remain safe even when that token is already gone.
            state.begin_processing("transcribe".into(), "fixture".into());
            let (capture_gate, capture) = gate();
            let (cleanup_gate, cleanup) = gate();
            *effects.capture_gate.lock().unwrap() = Some(capture_gate);
            *effects.cleanup_gate.lock().unwrap() = Some(cleanup_gate);
            crate::utils::cancel_current_operation(&app);
            deliver(&app, &mut state, &tx, &rx);
            capture.wait_entered();

            crate::actions::TranscribeAction {
                post_process: false,
            }
            .stop(&app, "transcribe", "fixture");
            let finished = rx.recv_timeout(TIMEOUT).unwrap();
            assert!(
                matches!(finished, Command::ProcessingFinished),
                "REGRESSION: cancellation completion preceded its cleanup"
            );
            dispatch_command(&app, &mut state, finished, &tx);
            assert_eq!(
                state.stage,
                Stage::Processing,
                "REGRESSION: no-token Stop published Idle during capture cleanup"
            );
            dispatch_command(&app, &mut state, input(), &tx);
            assert_eq!(effects.starts.load(Ordering::SeqCst), 1);
            assert!(app.state::<Arc<AudioRecordingManager>>().is_recording());

            capture.release();
            cleanup.wait_entered();
            assert!(!app.state::<Arc<AudioRecordingManager>>().is_recording());
            assert_eq!(
                state.stage,
                Stage::Processing,
                "REGRESSION: capture completion bypassed cancel_stream/UI cleanup"
            );
            assert!(
                rx.try_recv().is_err(),
                "REGRESSION: cancellation finished before stream/UI cleanup"
            );
            assert_eq!(effects.starts.load(Ordering::SeqCst), 1);

            cleanup.release();
            deliver(&app, &mut state, &tx, &rx);
            assert_eq!(effects.starts.load(Ordering::SeqCst), 2);
            assert_eq!(state.stage, Stage::Recording("transcribe".into()));
            assert!(app.state::<Arc<AudioRecordingManager>>().is_recording());
            assert!(effects.overlay_visible.load(Ordering::SeqCst));
            assert!(effects.tray_recording.load(Ordering::SeqCst));
            assert!(
                rx.try_recv().is_err(),
                "REGRESSION: old Cancel left a late lifecycle notification"
            );
        }

        #[test]
        fn actual_failed_start_does_not_claim_an_older_active_capture() {
            let (app, effects, tx, _) = fixture();
            let manager = app.state::<Arc<AudioRecordingManager>>();
            assert!(manager
                .try_start_recording("older", crate::VadPolicy::Disabled)
                .is_ok());
            assert!(manager.is_recording());
            let mut state = CoordinatorState::new();
            dispatch_command(&app, &mut state, input(), &tx);
            assert_eq!(
                state.stage,
                Stage::Idle,
                "REGRESSION: rejected Start claimed the old recording as success"
            );
            assert!(manager.is_recording());
            assert_eq!(effects.starts.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn repeated_cancel_uses_one_worker_and_discards_queued_restart() {
            let (app, effects, tx, rx) = fixture();
            let mut state = CoordinatorState::new();
            dispatch_command(&app, &mut state, input(), &tx);
            let (capture_gate, capture) = gate();
            *effects.capture_gate.lock().unwrap() = Some(capture_gate);
            crate::utils::cancel_current_operation(&app);
            deliver(&app, &mut state, &tx, &rx);
            capture.wait_entered();
            dispatch_command(&app, &mut state, input(), &tx);
            assert!(state.pending_press.is_some());
            crate::utils::cancel_current_operation(&app);
            deliver(&app, &mut state, &tx, &rx);
            assert!(state.pending_press.is_none());
            assert_eq!(effects.cancellations.load(Ordering::SeqCst), 1);

            capture.release();
            deliver(&app, &mut state, &tx, &rx);
            assert_eq!(state.stage, Stage::Idle);
            assert_eq!(effects.cancellations.load(Ordering::SeqCst), 1);
            assert_eq!(effects.capture_stops.load(Ordering::SeqCst), 1);
            assert_eq!(effects.starts.load(Ordering::SeqCst), 1);
            dispatch_command(&app, &mut state, input(), &tx);
            assert_eq!(effects.starts.load(Ordering::SeqCst), 2);
            assert_eq!(state.stage, Stage::Recording("transcribe".into()));
        }
    }
}
use transcription_coordinator::TranscriptionCoordinator;
