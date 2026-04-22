//! Decibel Counter — Tauri 2.0 backend
//!
//! Opens the system's default microphone via `cpal`, computes RMS-based dB
//! levels on every audio callback, and streams the result to the React
//! frontend through Tauri's event system.
//!
//! # Signal-processing maths
//!
//! **Root Mean Square (RMS)**
//!
//! Given a PCM buffer of *N* normalised samples $x_i \in [-1, 1]$:
//!
//! $$RMS = \sqrt{\frac{1}{N} \sum_{i=1}^{N} x_i^2}$$
//!
//! **dBFS → gaming-scale dB conversion**
//!
//! Standard full-scale dBFS is negative (silence ≈ −100 dBFS, clipping = 0 dBFS).
//! The +100 offset maps the range onto a positive scale that suits a gaming HUD:
//! silence ≈ 0, normal speech ≈ 60–70, shouting ≈ 85–95.
//!
//! $$dB = 20 \cdot \log_{10}(RMS) + 100$$
//!
//! **Auto-calibration threshold**
//!
//! Given *N* active-speech samples (silence stripped):
//!
//! $$\mu = \frac{1}{N}\sum_{i=1}^{N} x_i$$
//!
//! $$\sigma = \sqrt{\frac{1}{N}\sum_{i=1}^{N}(x_i - \mu)^2}$$
//!
//! $$\text{Threshold} = \mu + 1.5\,\sigma$$
//!
//! This covers ≈93 % of a Gaussian distribution, so only genuine spikes
//! above the speaker's normal range trigger the alert.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

// ─── IPC payloads ────────────────────────────────────────────────────────────

/// Emitted to the frontend as the `"db-level"` event on every audio chunk.
#[derive(Clone, Serialize)]
struct DbPayload {
    /// Live dB reading, clamped to [0, 120].
    db: f32,
}

/// Emitted to the frontend as the `"calibration-result"` event after the
/// 10-second calibration window closes.
#[derive(Clone, Serialize)]
struct CalibrationResult {
    /// Recommended alert threshold in dB, clamped to [40, 95].
    threshold: f32,
    /// Mean of the active-speech samples.
    mean: f32,
    /// Standard deviation of the active-speech samples.
    std_dev: f32,
    /// Minimum active-speech sample captured.
    min: f32,
    /// Maximum active-speech sample captured.
    max: f32,
    /// Which mode was used: "normal" or "limit".
    mode: String,
}

// ─── App state ───────────────────────────────────────────────────────────────

/// Per-calibration-run state accumulated by the audio callbacks.
pub struct CalibrationState {
    /// `true` while the 10-second collection window is open.
    pub active: bool,
    /// Ordered list of gaming-scale dB readings captured during the window.
    pub samples: Vec<f32>,
}

/// Shared audio state, accessible from Tauri commands and the audio thread.
pub struct AudioState {
    /// When `true`, the audio callback suppresses dB events (mic stays open).
    pub muted: Arc<AtomicBool>,
    /// `true` while the audio capture thread has an active, playing stream.
    pub running: Arc<AtomicBool>,
    /// When set to `true`, the audio capture loop exits cleanly so the thread
    /// can be restarted with a different device.
    pub stop_flag: Arc<AtomicBool>,
    /// Calibration accumulator — written by the audio thread, read by the
    /// calibration timer thread.
    pub calibration: Arc<Mutex<CalibrationState>>,
    /// Selected input device name. `None` = OS default.
    pub device_name: Arc<Mutex<Option<String>>>,
}

// ─── Signal processing ───────────────────────────────────────────────────────

/// Computes the RMS of a normalised f32 PCM buffer, then converts it to a
/// positive dB scale suitable for a gaming HUD.
///
/// Returns `0.0` for empty or all-silent buffers.
fn rms_to_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let mean_sq: f32 =
        samples.iter().map(|&x| x * x).sum::<f32>() / samples.len() as f32;

    let rms = mean_sq.sqrt();
    if rms <= 0.0 {
        return 0.0;
    }

    let db = 20.0_f32.mul_add(rms.log10(), 100.0);
    db.clamp(0.0, 120.0)
}

// ─── Calibration algorithm ───────────────────────────────────────────────────

const SILENCE_FLOOR_DB: f32 = 10.0;
const MIN_ACTIVE_SAMPLES: usize = 10;
const CALIBRATION_FALLBACK_DB: f32 = 60.0;

struct ThresholdStats {
    threshold: f32,
    mean: f32,
    std_dev: f32,
    min: f32,
    max: f32,
}

/// Calibration mode — controls how the threshold is derived from samples.
///
/// - `Normal`: user speaks at their everyday gaming volume; threshold is set
///   *above* that range at μ + 1.5σ so only genuine spikes trigger an alert.
/// - `Limit`: user speaks at the loudest they want to allow; threshold is set
///   to the mean of what they recorded, matching that exact level.
#[derive(Clone, PartialEq)]
pub enum CalibMode {
    Normal,
    Limit,
}

fn compute_threshold_stats(samples: &[f32], mode: &CalibMode) -> ThresholdStats {
    let active: Vec<f32> = samples
        .iter()
        .copied()
        .filter(|&s| s > SILENCE_FLOOR_DB)
        .collect();

    if active.len() < MIN_ACTIVE_SAMPLES {
        return ThresholdStats {
            threshold: CALIBRATION_FALLBACK_DB,
            mean: 0.0,
            std_dev: 0.0,
            min: 0.0,
            max: 0.0,
        };
    }

    let n = active.len() as f32;
    let mean: f32 = active.iter().sum::<f32>() / n;
    let variance: f32 = active.iter().map(|&x| (x - mean).powi(2)).sum::<f32>() / n;
    let std_dev = variance.sqrt();

    let min = active.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = active.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

    let raw_threshold = match mode {
        CalibMode::Limit => mean,
        CalibMode::Normal => {
            if std_dev < 1.0 {
                let mut sorted = active.clone();
                sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let idx = ((sorted.len() as f32) * 0.90) as usize;
                sorted[idx.min(sorted.len() - 1)]
            } else {
                mean + 1.5 * std_dev
            }
        }
    };

    ThresholdStats {
        threshold: raw_threshold.clamp(40.0, 95.0),
        mean,
        std_dev,
        min,
        max,
    }
}

// ─── Audio capture thread ────────────────────────────────────────────────────

/// Maximum rate at which dB events are emitted to the frontend.
/// The audio callback fires ~50–200×/second depending on device buffer size.
/// Throttling to 10 Hz caps Tauri/WebView2 IPC overhead to near-zero so the
/// audio thread has no measurable impact on game frame times.
/// Calibration samples are still accumulated at full rate for accuracy.
const EMIT_INTERVAL_MS: u64 = 100;

/// Spawns a dedicated background thread that opens the selected (or default)
/// input device, matches the native sample format, and on every audio callback:
///   - appends the dB reading to the calibration buffer (full rate), AND
///   - emits `"db-level"` at most once per EMIT_INTERVAL_MS (10 Hz throttle).
///
/// The thread exits cleanly when `stop_flag` is set to `true`, which allows
/// the audio device to be switched without restarting the whole app.
pub fn start_audio_listener(
    app: AppHandle,
    muted: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    calibration: Arc<Mutex<CalibrationState>>,
    device_name: Arc<Mutex<Option<String>>>,
) {
    // Guard against double-spawn.
    if running.swap(true, Ordering::SeqCst) {
        return;
    }

    std::thread::Builder::new()
        .name("db-audio-capture".into())
        .spawn(move || {
            let host = cpal::default_host();

            // ── Resolve input device ──────────────────────────────────────
            // If a device name is stored, try to match it in the host's
            // enumerated list. Fall back to the OS default on any failure.
            let requested = device_name.lock().unwrap().clone();
            let device = match requested {
                Some(ref name) => {
                    let found = host
                        .input_devices()
                        .ok()
                        .and_then(|mut devs| {
                            devs.find(|d| d.name().map_or(false, |n| n == *name))
                        });
                    if found.is_none() {
                        eprintln!("[dB Meter] Device '{}' not found, falling back to default.", name);
                    }
                    found.or_else(|| host.default_input_device())
                }
                None => host.default_input_device(),
            };

            let device = match device {
                Some(d) => d,
                None => {
                    eprintln!("[dB Meter] No microphone found.");
                    running.store(false, Ordering::SeqCst);
                    let _ = app.emit("mic-error", ());
                    return;
                }
            };

            let supported = match device.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[dB Meter] Could not read device config: {e}");
                    running.store(false, Ordering::SeqCst);
                    let _ = app.emit("mic-error", ());
                    return;
                }
            };

            eprintln!(
                "[dB Meter] Device: {:?}  format: {:?}  rate: {} Hz  emit_interval: {}ms",
                device.name().unwrap_or_default(),
                supported.sample_format(),
                supported.sample_rate().0,
                EMIT_INTERVAL_MS,
            );

            let app = Arc::new(app);

            // ── Emit throttle ─────────────────────────────────────────────
            // Lock-free AtomicU64 tracks ms since thread start.
            // An occasional double-emit on a benign race is harmless.
            let audio_start  = Instant::now();
            let last_emit_ms = Arc::new(AtomicU64::new(0));

            // ── Macro: calibration buffer + throttled IPC emit ────────────
            macro_rules! on_db {
                ($app:expr, $muted:expr, $cal:expr, $last:expr, $start:expr, $db:expr) => {{
                    let db: f32 = $db;

                    // Always push to calibration buffer at full rate (try_lock —
                    // never block the real-time audio callback).
                    if let Ok(mut cal) = $cal.try_lock() {
                        if cal.active {
                            cal.samples.push(db);
                        }
                    }

                    // Throttled IPC: emit at most 10×/second.
                    if !$muted.load(Ordering::Relaxed) {
                        let now_ms = $start.elapsed().as_millis() as u64;
                        let last   = $last.load(Ordering::Relaxed);
                        if now_ms.saturating_sub(last) >= EMIT_INTERVAL_MS {
                            $last.store(now_ms, Ordering::Relaxed);
                            let _ = $app.emit("db-level", DbPayload { db });
                        }
                    }
                }};
            }

            // ── Build a stream matching the device's native format ────────
            let stream_result = match supported.sample_format() {

                // ── F64 — CoreAudio on macOS sometimes reports this ──────
                cpal::SampleFormat::F64 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let last_cb     = Arc::clone(&last_emit_ms);
                    let start_cb    = audio_start;
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[f64], _: &cpal::InputCallbackInfo| {
                            let floats: Vec<f32> =
                                data.iter().map(|&s| s as f32).collect();
                            on_db!(app_cb, muted_cb, cal_cb, last_cb, start_cb, rms_to_db(&floats));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                // ── I32 — CoreAudio + some WASAPI devices ─────────────────
                cpal::SampleFormat::I32 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let last_cb     = Arc::clone(&last_emit_ms);
                    let start_cb    = audio_start;
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[i32], _: &cpal::InputCallbackInfo| {
                            let floats: Vec<f32> = data
                                .iter()
                                .map(|&s| s as f32 / i32::MAX as f32)
                                .collect();
                            on_db!(app_cb, muted_cb, cal_cb, last_cb, start_cb, rms_to_db(&floats));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                cpal::SampleFormat::F32 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let last_cb     = Arc::clone(&last_emit_ms);
                    let start_cb    = audio_start;
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            on_db!(app_cb, muted_cb, cal_cb, last_cb, start_cb, rms_to_db(data));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                cpal::SampleFormat::I16 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let last_cb     = Arc::clone(&last_emit_ms);
                    let start_cb    = audio_start;
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            let floats: Vec<f32> = data
                                .iter()
                                .map(|&s| s as f32 / i16::MAX as f32)
                                .collect();
                            on_db!(app_cb, muted_cb, cal_cb, last_cb, start_cb, rms_to_db(&floats));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                cpal::SampleFormat::U16 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let last_cb     = Arc::clone(&last_emit_ms);
                    let start_cb    = audio_start;
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            let floats: Vec<f32> = data
                                .iter()
                                .map(|&s| (s as f32 - 32_768.0) / 32_768.0)
                                .collect();
                            on_db!(app_cb, muted_cb, cal_cb, last_cb, start_cb, rms_to_db(&floats));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                fmt => {
                    eprintln!("[dB Meter] Unsupported sample format: {fmt:?}");
                    running.store(false, Ordering::SeqCst);
                    let _ = app.emit("mic-error", ());
                    return;
                }
            };

            // ── Start streaming ───────────────────────────────────────────
            match stream_result {
                Ok(stream) => {
                    if let Err(e) = stream.play() {
                        eprintln!("[dB Meter] Failed to start stream: {e}");
                        running.store(false, Ordering::SeqCst);
                        let _ = app.emit("mic-error", ());
                        return;
                    }
                    eprintln!("[dB Meter] Audio stream running.");
                    // Poll stop_flag every 50ms. This lets the device-switch
                    // command cleanly stop this thread before starting a new one.
                    while !stop_flag.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    drop(stream);
                    running.store(false, Ordering::SeqCst);
                    eprintln!("[dB Meter] Audio stream stopped.");
                }
                Err(e) => {
                    eprintln!("[dB Meter] Failed to build stream: {e}");
                    running.store(false, Ordering::SeqCst);
                    let _ = app.emit("mic-error", ());
                }
            }
        })
        .expect("Failed to spawn audio-capture thread");
}

// ─── Tauri commands ──────────────────────────────────────────────────────────

/// Toggle mute on/off. Returns the **new** muted state (`true` = muted).
#[tauri::command]
fn toggle_mute(state: State<AudioState>) -> bool {
    let was_muted = state.muted.load(Ordering::SeqCst);
    state.muted.store(!was_muted, Ordering::SeqCst);
    !was_muted
}

/// Re-attempt audio capture after a mic disconnect or permission error.
/// Does nothing if the stream is already running.
#[tauri::command]
fn retry_audio(app: AppHandle, state: State<AudioState>) {
    start_audio_listener(
        app,
        Arc::clone(&state.muted),
        Arc::clone(&state.running),
        Arc::clone(&state.stop_flag),
        Arc::clone(&state.calibration),
        Arc::clone(&state.device_name),
    );
}

/// Returns the names of all available audio input devices.
/// The frontend uses this to populate the device selector.
/// An empty string in the returned list is never emitted — the frontend
/// adds its own "System Default" entry that maps to an empty invoke arg.
#[tauri::command]
fn list_audio_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
        Err(_) => vec![],
    }
}

/// Switch to a different audio input device without restarting the app.
///
/// Pass an empty string to revert to the OS default device.
///
/// Internally: sets the stop flag so the current audio thread exits cleanly,
/// waits up to 300ms for it to finish, then restarts the listener with the
/// new device.  This is done on a background thread so the command returns
/// immediately without blocking the Tauri event loop.
#[tauri::command]
fn set_audio_device(app: AppHandle, state: State<AudioState>, name: String) {
    // Store the new device preference.
    {
        let mut dev = state.device_name.lock().unwrap();
        *dev = if name.is_empty() { None } else { Some(name.clone()) };
    }
    eprintln!("[dB Meter] Switching device → {}", if name.is_empty() { "System Default" } else { &name });

    // Signal the current audio thread to stop.
    state.stop_flag.store(true, Ordering::SeqCst);

    // Clone everything needed to restart on a helper thread.
    let muted       = Arc::clone(&state.muted);
    let running     = Arc::clone(&state.running);
    let stop_flag   = Arc::clone(&state.stop_flag);
    let calibration = Arc::clone(&state.calibration);
    let device_name = Arc::clone(&state.device_name);

    std::thread::spawn(move || {
        // Wait for the old audio thread to set running=false (max 300ms).
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(50));
            if !running.load(Ordering::SeqCst) {
                break;
            }
        }
        // Clear stop flag and launch the new stream.
        stop_flag.store(false, Ordering::SeqCst);
        start_audio_listener(app, muted, running, stop_flag, calibration, device_name);
    });
}

/// Start a 10-second auto-calibration window.
///
/// `mode` is either `"normal"` (speak at everyday volume; threshold = μ+1.5σ)
/// or `"limit"` (speak at the loudest you want to allow; threshold = mean).
///
/// **Debounce:** calling while a calibration is in progress is a no-op.
#[tauri::command]
fn start_calibration(app: AppHandle, state: State<AudioState>, mode: String) {
    {
        let mut cal = state.calibration.lock().unwrap();
        if cal.active {
            return;
        }
        cal.active = true;
        cal.samples.clear();
    }

    let calibration = Arc::clone(&state.calibration);
    let calib_mode = if mode == "limit" { CalibMode::Limit } else { CalibMode::Normal };

    std::thread::Builder::new()
        .name("db-calibration-timer".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(10));

            let (stats, mode_str) = {
                let mut cal = calibration.lock().unwrap();
                cal.active = false;
                let s = compute_threshold_stats(&cal.samples, &calib_mode);
                let m = if calib_mode == CalibMode::Limit { "limit" } else { "normal" };
                (s, m.to_string())
            };

            eprintln!(
                "[dB Meter] Calibration ({mode_str}) — μ={:.1} σ={:.1} min={:.1} max={:.1} → threshold={:.1} dB",
                stats.mean, stats.std_dev, stats.min, stats.max, stats.threshold
            );

            let _ = app.emit(
                "calibration-result",
                CalibrationResult {
                    threshold: stats.threshold,
                    mean:      stats.mean,
                    std_dev:   stats.std_dev,
                    min:       stats.min,
                    max:       stats.max,
                    mode:      mode_str,
                },
            );
        })
        .expect("Failed to spawn calibration timer thread");
}

/// Quit the application entirely on all platforms.
///
/// `getCurrentWindow().close()` in the frontend only hides the window on
/// macOS — the process stays alive in the Dock. Calling `app.exit(0)` from
/// Rust guarantees the process terminates on every OS.
#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

// ─── App entry point ─────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let muted       = Arc::new(AtomicBool::new(false));
    let running     = Arc::new(AtomicBool::new(false));
    let stop_flag   = Arc::new(AtomicBool::new(false));
    let calibration = Arc::new(Mutex::new(CalibrationState {
        active:  false,
        samples: Vec::new(),
    }));
    let device_name = Arc::new(Mutex::new(None::<String>));

    tauri::Builder::default()
        .manage(AudioState {
            muted:       Arc::clone(&muted),
            running:     Arc::clone(&running),
            stop_flag:   Arc::clone(&stop_flag),
            calibration: Arc::clone(&calibration),
            device_name: Arc::clone(&device_name),
        })
        .invoke_handler(tauri::generate_handler![
            toggle_mute,
            retry_audio,
            list_audio_devices,
            set_audio_device,
            start_calibration,
            quit_app,
        ])
        .setup(move |app| {
            start_audio_listener(app.handle().clone(), muted, running, stop_flag, calibration, device_name);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Error while running Tauri application");
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{compute_threshold_stats, rms_to_db, CalibMode, CALIBRATION_FALLBACK_DB};

    #[test]
    fn silence_returns_zero() {
        assert_eq!(rms_to_db(&[0.0_f32; 256]), 0.0);
    }

    #[test]
    fn empty_buffer_returns_zero() {
        assert_eq!(rms_to_db(&[]), 0.0);
    }

    #[test]
    fn dc_full_scale_is_100_db() {
        let db = rms_to_db(&[1.0_f32; 256]);
        assert!((db - 100.0).abs() < 0.01, "expected 100 dB, got {db}");
    }

    #[test]
    fn full_scale_sine_near_97_db() {
        let samples: Vec<f32> = (0..1024)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / 64.0).sin())
            .collect();
        let db = rms_to_db(&samples);
        assert!((db - 97.0).abs() < 1.0, "expected ~97 dB, got {db}");
    }

    #[test]
    fn clipping_is_clamped_to_120() {
        let db = rms_to_db(&[2.0_f32; 256]);
        assert_eq!(db, 120.0);
    }

    #[test]
    fn result_never_negative() {
        let db = rms_to_db(&[0.001_f32; 256]);
        assert!(db >= 0.0, "dB must be >= 0, got {db}");
    }

    #[test]
    fn single_sample_does_not_panic() {
        let db = rms_to_db(&[0.5_f32]);
        assert!(db >= 0.0 && db <= 120.0);
    }

    #[test]
    fn empty_samples_returns_fallback() {
        let stats = compute_threshold_stats(&[], &CalibMode::Normal);
        assert_eq!(stats.threshold, CALIBRATION_FALLBACK_DB);
    }

    #[test]
    fn all_silence_returns_fallback() {
        let samples = vec![5.0_f32; 200];
        let stats = compute_threshold_stats(&samples, &CalibMode::Normal);
        assert_eq!(stats.threshold, CALIBRATION_FALLBACK_DB);
    }

    #[test]
    fn normal_mode_threshold_above_mean() {
        let samples: Vec<f32> = (0..500)
            .map(|i| 65.0 + 5.0 * ((i as f32 * 0.1).sin()))
            .collect();
        let stats = compute_threshold_stats(&samples, &CalibMode::Normal);
        assert!(stats.threshold >= 40.0 && stats.threshold <= 95.0);
        assert!(stats.threshold > stats.mean, "normal mode threshold must be above mean");
    }

    #[test]
    fn limit_mode_threshold_equals_mean() {
        let samples: Vec<f32> = (0..500)
            .map(|i| 75.0 + 3.0 * ((i as f32 * 0.1).sin()))
            .collect();
        let stats = compute_threshold_stats(&samples, &CalibMode::Limit);
        assert!((stats.threshold - stats.mean).abs() < 1.0,
            "limit mode threshold should equal mean, got threshold={} mean={}", stats.threshold, stats.mean);
    }

    #[test]
    fn constant_input_uses_percentile_fallback() {
        let samples = vec![70.0_f32; 200];
        let stats = compute_threshold_stats(&samples, &CalibMode::Normal);
        assert!((stats.threshold - 70.0).abs() < 1.0, "expected ~70 dB, got {}", stats.threshold);
    }

    #[test]
    fn threshold_always_clamped_to_valid_range() {
        let loud = vec![98.0_f32; 200];
        let stats = compute_threshold_stats(&loud, &CalibMode::Normal);
        assert!(stats.threshold <= 95.0);

        let quiet = vec![15.0_f32; 200];
        let quiet_stats = compute_threshold_stats(&quiet, &CalibMode::Limit);
        assert!(quiet_stats.threshold >= 40.0);
    }

    #[test]
    fn min_max_captured_correctly() {
        let samples: Vec<f32> = (0..200).map(|i| 50.0 + (i % 30) as f32).collect();
        let stats = compute_threshold_stats(&samples, &CalibMode::Normal);
        assert!(stats.min <= stats.mean, "min must be <= mean");
        assert!(stats.max >= stats.mean, "max must be >= mean");
        assert!(stats.min < stats.max, "min must be < max for varied input");
    }
}
