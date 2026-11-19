use super::recording_output::{OutputOptions, RecordingOutputController};
use crate::audio_toolkit::{
    list_input_devices,
    vad::{
        SmoothedVad, VAD_OFFLINE_HANGOVER_FRAMES, VAD_ONSET_FRAMES, VAD_PREFILL_FRAMES,
        VAD_STREAMING_HANGOVER_FRAMES,
    },
    AudioRecorder, SileroVad, VadPolicy,
};
use crate::helpers::clamshell;
use crate::managers::transcription::StreamRouter;
use crate::settings::{get_settings, AppSettings};
use crate::utils;
use log::{debug, error, info, warn};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Manager;

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const VAD_THRESHOLD: f32 = 0.3;

const WHISPER_SAMPLE_RATE: usize = 16000;

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone, Debug)]
pub enum RecordingState {
    Idle,
    Recording { binding_id: String, generation: u64 },
    Stopping { generation: u64 },
}

#[derive(Clone, Debug)]
pub enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

/* ──────────────────────────────────────────────────────────────── */

fn create_audio_recorder(
    vad_path: &Path,
    app_handle: &tauri::AppHandle,
    stream_router: Arc<StreamRouter>,
) -> Result<AudioRecorder, anyhow::Error> {
    // A single Silero engine covers both the offline and streaming policies (never
    // active at once within a recording), so the recorder reconfigures its
    // hangover tail per session rather than keeping two ONNX sessions resident.
    let silero = SileroVad::new(vad_path, VAD_THRESHOLD)
        .map_err(|e| anyhow::anyhow!("Failed to create SileroVad: {}", e))?;
    let smoothed_vad = SmoothedVad::new(
        Box::new(silero),
        VAD_PREFILL_FRAMES,
        VAD_OFFLINE_HANGOVER_FRAMES,
        VAD_ONSET_FRAMES,
    );

    // Recorder with VAD, a spectrum-level callback that forwards level updates to
    // the frontend, and an audio-frame callback that feeds live streaming via a
    // shared `StreamRouter` (captured directly, not via Tauri state — see its docs).
    let recorder = AudioRecorder::new()
        .map_err(|e| anyhow::anyhow!("Failed to create AudioRecorder: {}", e))?
        .with_vad(
            Box::new(smoothed_vad),
            VAD_OFFLINE_HANGOVER_FRAMES,
            VAD_STREAMING_HANGOVER_FRAMES,
        )
        .with_level_callback({
            let app_handle = app_handle.clone();
            move |levels| {
                utils::emit_levels(&app_handle, &levels);
            }
        })
        .with_audio_callback({
            let router = stream_router;
            move |frame| {
                router.feed(frame);
            }
        });

    Ok(recorder)
}

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone)]
pub struct AudioRecordingManager {
    state: Arc<Mutex<RecordingState>>,
    mode: Arc<Mutex<MicrophoneMode>>,
    app_handle: tauri::AppHandle,

    recorder: Arc<Mutex<Option<AudioRecorder>>>,
    is_open: Arc<Mutex<bool>>,
    is_recording: Arc<Mutex<bool>>,
    recording_output: RecordingOutputController,
    capture_generation: Arc<AtomicU64>,
    close_generation: Arc<AtomicU64>,
    cancel_generation: Arc<AtomicU64>,
    stream_router: Arc<StreamRouter>,
    /// Resolution of a *named* microphone (selected or clamshell) to its cpal
    /// device, cached so on-demand recording starts skip the full device
    /// enumeration (~40-110ms). Keyed by the resolved name, so a settings
    /// change misses naturally; cleared when an open fails (device unplugged)
    /// so the retry re-enumerates. The system-default case is never cached —
    /// the recorder resolves the current default itself, cheaply.
    cached_device: Arc<Mutex<Option<(String, cpal::Device)>>>,
}

impl AudioRecordingManager {
    /* ---------- construction ------------------------------------------------ */

    pub fn new(
        app: &tauri::AppHandle,
        stream_router: Arc<StreamRouter>,
    ) -> Result<Self, anyhow::Error> {
        let settings = get_settings(app);
        let mode = if settings.always_on_microphone {
            MicrophoneMode::AlwaysOn
        } else {
            MicrophoneMode::OnDemand
        };

        let manager = Self {
            state: Arc::new(Mutex::new(RecordingState::Idle)),
            mode: Arc::new(Mutex::new(mode.clone())),
            app_handle: app.clone(),

            recorder: Arc::new(Mutex::new(None)),
            is_open: Arc::new(Mutex::new(false)),
            is_recording: Arc::new(Mutex::new(false)),
            recording_output: RecordingOutputController::new(),
            capture_generation: Arc::new(AtomicU64::new(0)),
            close_generation: Arc::new(AtomicU64::new(0)),
            cancel_generation: Arc::new(AtomicU64::new(0)),
            stream_router,
            cached_device: Arc::new(Mutex::new(None)),
        };

        // Always-on?  Open immediately.
        if matches!(mode, MicrophoneMode::AlwaysOn) {
            manager.start_microphone_stream()?;
        }

        Ok(manager)
    }

    /* ---------- helper methods --------------------------------------------- */

    /// The microphone name the settings ask for, or `None` for the system
    /// default. Only runs the clamshell probe (an `ioreg` subprocess, ~10-20ms)
    /// when a clamshell microphone is actually configured.
    fn desired_device_name(&self, settings: &AppSettings) -> Option<String> {
        if settings.clamshell_microphone.is_some() {
            let clamshell_started = Instant::now();
            let is_clamshell = clamshell::is_clamshell().unwrap_or(false);
            debug!(
                "device resolve: clamshell_check={:?} (clamshell={})",
                clamshell_started.elapsed(),
                is_clamshell
            );
            if is_clamshell {
                return settings.clamshell_microphone.clone();
            }
        }
        settings.selected_microphone.clone()
    }

    pub fn invalidate_device_cache(&self) {
        *self.cached_device.lock().unwrap() = None;
    }

    fn get_effective_microphone_device(&self, settings: &AppSettings) -> Option<cpal::Device> {
        let device_name = match self.desired_device_name(settings) {
            Some(name) => name,
            None => {
                debug!("device resolve: no mic configured -> system default");
                return None;
            }
        };

        // Cache hit: skip the full enumeration. A stale device (unplugged)
        // fails at open, where the caller invalidates and retries fresh.
        if let Some((cached_name, device)) = self.cached_device.lock().unwrap().as_ref() {
            if *cached_name == device_name {
                debug!("device resolve: cache hit for '{}'", device_name);
                return Some(device.clone());
            }
        }

        // Find the device by name
        let enumerate_started = Instant::now();
        let device = match list_input_devices() {
            Ok(devices) => devices
                .into_iter()
                .find(|d| d.name == device_name)
                .map(|d| d.device),
            Err(e) => {
                debug!("Failed to list devices, using default: {}", e);
                None
            }
        };
        debug!(
            "device resolve: enumerate={:?} (found={})",
            enumerate_started.elapsed(),
            device.is_some()
        );
        if let Some(d) = &device {
            *self.cached_device.lock().unwrap() = Some((device_name, d.clone()));
        }
        device
    }

    fn schedule_lazy_close(&self) {
        let gen = self.close_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.app_handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(STREAM_IDLE_TIMEOUT);
            let rm = app.state::<Arc<AudioRecordingManager>>();
            // Hold state lock across the check AND close to serialize against
            // try_start_recording, preventing a race where the stream is closed
            // under an active recording.
            let state = rm.state.lock().unwrap();
            if rm.close_generation.load(Ordering::SeqCst) == gen
                && matches!(*state, RecordingState::Idle)
            {
                // stop_microphone_stream does not acquire the state lock,
                // so holding it here is safe (no deadlock).
                info!(
                    "Closing idle microphone stream after {:?}",
                    STREAM_IDLE_TIMEOUT
                );
                rm.stop_microphone_stream();
            }
        });
    }

    /* ---------- microphone life-cycle -------------------------------------- */

    pub fn apply_recording_output(&self, generation: u64) {
        self.recording_output.ready(generation);
    }

    pub fn restore_output_before_exit(&self) {
        self.cancel_recording();
        let _state = self.state.lock().unwrap();
        self.stop_microphone_stream();
        self.recording_output.restore_before_exit();
    }

    pub fn preload_vad(&self) -> Result<(), anyhow::Error> {
        let mut recorder_opt = self.recorder.lock().unwrap();
        if recorder_opt.is_none() {
            let vad_path = self
                .app_handle
                .path()
                .resolve(
                    "resources/models/silero_vad_v4.onnx",
                    tauri::path::BaseDirectory::Resource,
                )
                .map_err(|e| anyhow::anyhow!("Failed to resolve VAD path: {}", e))?;
            *recorder_opt = Some(create_audio_recorder(
                &vad_path,
                &self.app_handle,
                Arc::clone(&self.stream_router),
            )?);
        }
        Ok(())
    }

    pub fn start_microphone_stream(&self) -> Result<(), anyhow::Error> {
        let mut open_flag = self.is_open.lock().unwrap();
        if *open_flag {
            debug!("Microphone stream already active");
            return Ok(());
        }

        let start_time = Instant::now();

        // Get the selected device from settings, considering clamshell mode.
        // No pre-flight enumeration here: when nothing is configured the
        // recorder resolves the system default itself, and a machine with no
        // input devices at all fails inside open() with the same
        // "No input device found" error this used to check for.
        let settings = get_settings(&self.app_handle);
        let resolve_started = Instant::now();
        let selected_device = self.get_effective_microphone_device(&settings);
        let resolve_elapsed = resolve_started.elapsed();

        // Ensure VAD is loaded if it wasn't for whatever reason
        let vad_started = Instant::now();
        self.preload_vad()?;
        let vad_elapsed = vad_started.elapsed();

        let open_started = Instant::now();
        let mut recorder_opt = self.recorder.lock().unwrap();
        if let Some(rec) = recorder_opt.as_mut() {
            if let Err(first_err) = rec.open(selected_device.clone()) {
                // A cached device or config may have gone stale (unplugged,
                // rate/format changed). Re-resolve from a fresh enumeration and
                // retry once before surfacing the error.
                warn!("Recorder open failed ({first_err}); re-resolving device and retrying once");
                self.invalidate_device_cache();
                let fresh_device = self.get_effective_microphone_device(&settings);
                rec.open(fresh_device)
                    .map_err(|e| anyhow::anyhow!("Failed to open recorder: {}", e))?;
            }
        }
        debug!(
            "mic stream breakdown: device_resolve={:?} vad_ensure={:?} open={:?}",
            resolve_elapsed,
            vad_elapsed,
            open_started.elapsed()
        );

        *open_flag = true;
        // This timing covers through cpal's stream.play() returning — i.e. the
        // point cpal surfaces as "stream running." It does NOT guarantee the
        // host audio device is producing samples yet; the first input callback
        // fires asynchronously one buffer period later (hardware dependent,
        // typically ~10–200ms on macOS, longer on Bluetooth/USB).
        info!(
            "Microphone stream initialized in {:?}",
            start_time.elapsed()
        );
        Ok(())
    }

    pub fn stop_microphone_stream(&self) {
        let mut open_flag = self.is_open.lock().unwrap();
        if !*open_flag {
            return;
        }

        self.capture_generation.fetch_add(1, Ordering::AcqRel);
        self.recording_output.close_after_capture(|| {
            if let Some(rec) = self.recorder.lock().unwrap().as_mut() {
                if *self.is_recording.lock().unwrap() {
                    let _ = rec.stop();
                    *self.is_recording.lock().unwrap() = false;
                }
                let _ = rec.close();
            }
        });

        *open_flag = false;
        debug!("Microphone stream stopped");
    }

    /* ---------- mode switching --------------------------------------------- */

    pub fn update_mode(&self, new_mode: MicrophoneMode) -> Result<(), anyhow::Error> {
        // Serialize stream close/open with recording start through its output
        // generation registration, not just through the is_open check.
        let state = self.state.lock().unwrap();
        let cur_mode = self.mode.lock().unwrap().clone();

        match (cur_mode, &new_mode) {
            (MicrophoneMode::AlwaysOn, MicrophoneMode::OnDemand) => {
                if matches!(*state, RecordingState::Idle) {
                    self.close_generation.fetch_add(1, Ordering::SeqCst);
                    self.stop_microphone_stream();
                }
            }
            (MicrophoneMode::OnDemand, MicrophoneMode::AlwaysOn) => {
                self.close_generation.fetch_add(1, Ordering::SeqCst);
                self.start_microphone_stream()?;
            }
            _ => {}
        }

        *self.mode.lock().unwrap() = new_mode;
        Ok(())
    }

    /* ---------- recording --------------------------------------------------- */

    pub fn try_start_recording(
        &self,
        binding_id: &str,
        vad_policy: VadPolicy,
    ) -> Result<u64, String> {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Idle = *state {
            // Invalidate any previous lazy close before this recording begins.
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                if let Err(e) = self.start_microphone_stream() {
                    let msg = format!("{e}");
                    error!("Failed to open microphone stream: {msg}");
                    return Err(msg);
                }
            }

            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                if rec.start(vad_policy).is_ok() {
                    let generation = self.capture_generation.fetch_add(1, Ordering::AcqRel) + 1;
                    *self.is_recording.lock().unwrap() = true;
                    *state = RecordingState::Recording {
                        binding_id: binding_id.to_string(),
                        generation,
                    };
                    let settings = get_settings(&self.app_handle);
                    self.recording_output.begin(
                        generation,
                        OutputOptions {
                            mute: settings.mute_while_recording,
                            pause: settings.pause_while_recording,
                            resume: settings.play_after_recording,
                        },
                    );
                    debug!("Recording started for binding {binding_id}");
                    return Ok(generation);
                }
            }
            Err("Recorder not available".to_string())
        } else {
            Err("Already recording".to_string())
        }
    }

    pub fn update_selected_device(&self) -> Result<(), anyhow::Error> {
        // Close/open and generation registration share the same state guard.
        let _state = self.state.lock().unwrap();
        // Device settings changed; drop the cached resolution so the next
        // open re-enumerates. (The name-keyed cache would miss anyway; this
        // just avoids holding a stale cpal::Device alive.)
        self.invalidate_device_cache();
        // If currently open, restart the microphone stream to use the new device
        if *self.is_open.lock().unwrap() {
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            self.stop_microphone_stream();
            self.start_microphone_stream()?;
        }
        Ok(())
    }

    /// Capture the stop token before dispatching asynchronous work. A delayed
    /// stop for an older recording must never stop a new one with the same hotkey.
    pub fn request_recording_stop(&self, binding_id: &str) -> Option<u64> {
        let state = self.state.lock().unwrap();
        let RecordingState::Recording {
            binding_id: active,
            generation,
        } = &*state
        else {
            return None;
        };
        if active != binding_id {
            return None;
        }
        self.invalidate_recording_readiness(*generation);
        Some(*generation)
    }

    /// Reject readiness work without restoring audio into the capture tail.
    fn invalidate_recording_readiness(&self, generation: u64) {
        self.capture_generation.fetch_add(1, Ordering::AcqRel);
        self.recording_output.stop_requested(generation);
    }

    pub fn is_recording_readiness_current(&self, generation: u64) -> bool {
        self.capture_generation.load(Ordering::Acquire) == generation
    }

    pub fn cancel_generation(&self) -> u64 {
        self.cancel_generation.load(Ordering::Acquire)
    }

    pub fn was_cancelled_since(&self, generation: u64) -> bool {
        self.cancel_generation.load(Ordering::Acquire) != generation
    }

    pub fn stop_recording(
        &self,
        binding_id: &str,
        generation: u64,
        cancel_generation: u64,
    ) -> Option<Vec<f32>> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            RecordingState::Recording {
                binding_id: active,
                generation: active_generation,
            } if active == binding_id && *active_generation == generation => {}
            _ => return None,
        }
        self.invalidate_recording_readiness(generation);
        *state = RecordingState::Stopping { generation };
        drop(state);

        let samples = self.recording_output.finish_after_capture(generation, || {
            // Keep both protections through the explicit capture tail. Cancel
            // only signals this loop; the stop owner performs the actual cleanup.
            let buffer_ms = get_settings(&self.app_handle).extra_recording_buffer_ms;
            if buffer_ms > 0 {
                debug!("Extra recording buffer: waiting {buffer_ms}ms before stopping");
                let started = Instant::now();
                let buffer = Duration::from_millis(buffer_ms);
                while started.elapsed() < buffer {
                    if self.was_cancelled_since(cancel_generation) {
                        debug!("Recording stop cancelled during extra buffer");
                        break;
                    }
                    let remaining = buffer.saturating_sub(started.elapsed());
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                }
            }

            let samples = if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                match rec.stop() {
                    Ok(buf) => buf,
                    Err(error) => {
                        error!("stop() failed: {error}");
                        Vec::new()
                    }
                }
            } else {
                error!("Recorder not available");
                Vec::new()
            };
            *self.is_recording.lock().unwrap() = false;
            samples
        });
        self.finish_recording_state(generation);

        if self.was_cancelled_since(cancel_generation) {
            debug!("Recording stop cancelled; discarding captured samples");
            return None;
        }
        samples.map(|mut samples| {
            if !samples.is_empty() && samples.len() < WHISPER_SAMPLE_RATE {
                samples.resize(WHISPER_SAMPLE_RATE * 5 / 4, 0.0);
            }
            samples
        })
    }

    /// Publish Idle only after capture, restoration and any immediate close.
    /// Otherwise a quick restart can be stopped by the previous recording's work.
    fn finish_recording_state(&self, generation: u64) {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, RecordingState::Stopping { generation: active } if active == generation)
        {
            return;
        }
        if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
            if get_settings(&self.app_handle).lazy_stream_close {
                self.schedule_lazy_close();
            } else {
                self.stop_microphone_stream();
            }
        }
        *state = RecordingState::Idle;
    }

    pub fn is_recording(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            RecordingState::Recording { .. } | RecordingState::Stopping { .. }
        )
    }

    /// Cancel uses the same capture-before-restoration path as normal Stop,
    /// even when an always-on or lazily closed microphone remains open.
    pub fn cancel_recording(&self) {
        self.cancel_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.lock().unwrap();
        let generation = match *state {
            RecordingState::Recording { generation, .. } => generation,
            RecordingState::Stopping { generation } => {
                self.invalidate_recording_readiness(generation);
                debug!("Cancellation requested while recording is stopping");
                return;
            }
            RecordingState::Idle => return,
        };
        self.invalidate_recording_readiness(generation);
        *state = RecordingState::Stopping { generation };
        drop(state);

        self.recording_output.finish_after_capture(generation, || {
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                let _ = rec.stop(); // Discard cancelled samples.
            }
            *self.is_recording.lock().unwrap() = false;
        });
        self.finish_recording_state(generation);
    }
}
