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
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
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
    /// Calibration accumulator — written by the audio thread, read by the
    /// calibration timer thread.
    pub calibration: Arc<Mutex<CalibrationState>>,
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

    // mean_square = (1/N) · Σ xᵢ²
    let mean_sq: f32 =
        samples.iter().map(|&x| x * x).sum::<f32>() / samples.len() as f32;

    let rms = mean_sq.sqrt();
    if rms <= 0.0 {
        return 0.0;
    }

    // dB = 20·log₁₀(RMS) + 100  (gaming-scale offset)
    let db = 20.0_f32.mul_add(rms.log10(), 100.0);
    db.clamp(0.0, 120.0)
}

// ─── Calibration algorithm ───────────────────────────────────────────────────

/// Minimum dB value treated as active speech (below = silence).
const SILENCE_FLOOR_DB: f32 = 10.0;

/// Minimum active-speech samples needed for statistical validity.
const MIN_ACTIVE_SAMPLES: usize = 10;

/// Returned when not enough data is available.
const CALIBRATION_FALLBACK_DB: f32 = 60.0;

/// Internal result of `compute_threshold_stats`.
struct ThresholdStats {
    threshold: f32,
    mean: f32,
    std_dev: f32,
    min: f32,
    max: f32,
}

/// Computes a recommended alert threshold from a 10-second sample set.
///
/// # Algorithm
///
/// **Step 1 — Strip silence**
/// Discard samples ≤ `SILENCE_FLOOR_DB` so that natural speech pauses
/// don't pull the mean down.
///
/// **Step 2 — Gaussian upper bound (primary path)**
///
/// ```text
/// μ  = (1/N) · Σ xᵢ                    (mean)
/// σ² = (1/N) · Σ (xᵢ − μ)²             (variance)
/// σ  = √σ²                              (standard deviation)
/// Threshold = μ + 1.5·σ
/// ```
///
/// 1.5σ covers ≈93 % of the speaker's Gaussian distribution, so only
/// genuine spikes above normal gaming volume trigger the alert.
///
/// **Step 3 — 90th-percentile fallback**
/// When σ < 1.0 the input is essentially constant (e.g., background hiss
/// or a pink-noise test signal).  In that case the Gaussian model is
/// degenerate, so we fall back to:
///
/// ```text
/// P₉₀ = samples_sorted[ ⌊0.90 · N⌋ ]
/// ```
///
/// **Step 4 — Clamp to [40, 95] dB**
/// Prevents pathological thresholds from muting all alerts or firing
/// permanently.
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
    // Step 1 — strip silence
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

    // μ = (1/N) · Σ xᵢ
    let mean: f32 = active.iter().sum::<f32>() / n;

    // σ = √( (1/N) · Σ (xᵢ − μ)² )
    let variance: f32 = active.iter().map(|&x| (x - mean).powi(2)).sum::<f32>() / n;
    let std_dev = variance.sqrt();

    let min = active.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = active.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

    let raw_threshold = match mode {
        // Limit mode: user spoke at the volume they want to warn at.
        // Threshold = mean of what they recorded.
        CalibMode::Limit => mean,

        // Normal mode: user spoke at everyday volume.
        // Threshold = μ + 1.5σ  (covers ≈93% of distribution)
        // Fallback to P₉₀ when σ < 1 (near-constant input).
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

    // Clamp to [40, 95] dB
    ThresholdStats {
        threshold: raw_threshold.clamp(40.0, 95.0),
        mean,
        std_dev,
        min,
        max,
    }
}

// ─── Audio capture thread ────────────────────────────────────────────────────

/// Spawns a dedicated background thread that opens the default input device,
/// matches the native sample format, and on every audio callback:
///   - emits `"db-level"` unless muted, AND
///   - appends the dB reading to the calibration buffer when active.
///
/// Uses `try_lock()` for the calibration mutex — the audio callback must
/// never block waiting for a lock, as that would cause glitches.
///
/// Guards against double-spawn: if `running` is already `true`, returns
/// immediately.
pub fn start_audio_listener(
    app: AppHandle,
    muted: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    calibration: Arc<Mutex<CalibrationState>>,
) {
    // Atomically set running = true; abort if already running.
    if running.swap(true, Ordering::SeqCst) {
        return;
    }

    std::thread::Builder::new()
        .name("db-audio-capture".into())
        .spawn(move || {
            let host = cpal::default_host();

            // ── Enumerate the default microphone ─────────────────────────
            let device = match host.default_input_device() {
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
                "[dB Meter] Device: {:?}  format: {:?}  rate: {} Hz",
                device.name().unwrap_or_default(),
                supported.sample_format(),
                supported.sample_rate().0,
            );

            let app = Arc::new(app);

            // ── Macro: emit dB + optionally record for calibration ───────
            //
            // Defined as a local closure to avoid copy-pasting across the
            // three format branches below.  Takes the computed dB value,
            // emits the event (unless muted), and tries to append to the
            // calibration buffer using try_lock — never blocking.
            macro_rules! on_db {
                ($app:expr, $muted:expr, $cal:expr, $db:expr) => {{
                    let db: f32 = $db;
                    if !$muted.load(Ordering::Relaxed) {
                        let _ = $app.emit("db-level", DbPayload { db });
                    }
                    // try_lock: skip this sample if the calibration thread
                    // holds the mutex — one missed frame is harmless.
                    if let Ok(mut cal) = $cal.try_lock() {
                        if cal.active {
                            cal.samples.push(db);
                        }
                    }
                }};
            }

            // ── Build a stream matching the device's native format ────────
            let stream_result = match supported.sample_format() {

                // ── F32 — already normalised ──────────────────────────────
                cpal::SampleFormat::F32 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            on_db!(app_cb, muted_cb, cal_cb, rms_to_db(data));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                // ── I16 — normalise to [-1.0, 1.0] ───────────────────────
                cpal::SampleFormat::I16 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            let floats: Vec<f32> = data
                                .iter()
                                .map(|&s| s as f32 / i16::MAX as f32)
                                .collect();
                            on_db!(app_cb, muted_cb, cal_cb, rms_to_db(&floats));
                        },
                        move |e| {
                            eprintln!("[dB Meter] Stream error: {e}");
                            running_err.store(false, Ordering::SeqCst);
                            let _ = app_err.emit("mic-error", ());
                        },
                        None,
                    )
                }

                // ── U16 — unsigned, centre at 32 768 ─────────────────────
                cpal::SampleFormat::U16 => {
                    let app_cb      = Arc::clone(&app);
                    let muted_cb    = Arc::clone(&muted);
                    let cal_cb      = Arc::clone(&calibration);
                    let running_err = Arc::clone(&running);
                    let app_err     = Arc::clone(&app);
                    device.build_input_stream(
                        &supported.into(),
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            let floats: Vec<f32> = data
                                .iter()
                                .map(|&s| (s as f32 - 32_768.0) / 32_768.0)
                                .collect();
                            on_db!(app_cb, muted_cb, cal_cb, rms_to_db(&floats));
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
                    // Park indefinitely — `stream` must stay alive.
                    std::thread::park();
                    drop(stream);
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
        Arc::clone(&state.calibration),
    );
}

/// Start a 10-second auto-calibration window.
///
/// `mode` is either `"normal"` (speak at everyday volume; threshold = μ+1.5σ)
/// or `"limit"` (speak at the loudest you want to allow; threshold = mean).
///
/// **Debounce:** calling while a calibration is in progress is a no-op.
#[tauri::command]
fn start_calibration(app: AppHandle, state: State<AudioState>, mode: String) {
    // ── Debounce / arm the collection window ─────────────────────────────
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

    // ── Timer thread — sleeps 10 s, reads samples, emits result ──────────
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

// ─── App entry point ─────────────────────────────────────────────────────────

/// Called by `main.rs`.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let muted       = Arc::new(AtomicBool::new(false));
    let running     = Arc::new(AtomicBool::new(false));
    let calibration = Arc::new(Mutex::new(CalibrationState {
        active:  false,
        samples: Vec::new(),
    }));

    tauri::Builder::default()
        .manage(AudioState {
            muted:       Arc::clone(&muted),
            running:     Arc::clone(&running),
            calibration: Arc::clone(&calibration),
        })
        .invoke_handler(tauri::generate_handler![
            toggle_mute,
            retry_audio,
            start_calibration,
        ])
        .setup(move |app| {
            start_audio_listener(app.handle().clone(), muted, running, calibration);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Error while running Tauri application");
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{compute_threshold_stats, rms_to_db, CalibMode, CALIBRATION_FALLBACK_DB};

    // ── rms_to_db ────────────────────────────────────────────────────────────

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
        // DC at amplitude 1.0 → RMS = 1.0 → 20·log₁₀(1) + 100 = 100 dB
        let db = rms_to_db(&[1.0_f32; 256]);
        assert!((db - 100.0).abs() < 0.01, "expected 100 dB, got {db}");
    }

    #[test]
    fn full_scale_sine_near_97_db() {
        // Full-scale sine has RMS = 1/√2 ≈ 0.707.
        // Expected: 20·log₁₀(0.707) + 100 ≈ 96.99 dB
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

    // ── compute_threshold_stats ──────────────────────────────────────────────

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
        // Normal mode: threshold = μ + 1.5σ, must be above mean
        let samples: Vec<f32> = (0..500)
            .map(|i| 65.0 + 5.0 * ((i as f32 * 0.1).sin()))
            .collect();
        let stats = compute_threshold_stats(&samples, &CalibMode::Normal);
        assert!(stats.threshold >= 40.0 && stats.threshold <= 95.0);
        assert!(stats.threshold > stats.mean, "normal mode threshold must be above mean");
    }

    #[test]
    fn limit_mode_threshold_equals_mean() {
        // Limit mode: user spoke at their limit level — threshold = mean
        let samples: Vec<f32> = (0..500)
            .map(|i| 75.0 + 3.0 * ((i as f32 * 0.1).sin()))
            .collect();
        let stats = compute_threshold_stats(&samples, &CalibMode::Limit);
        assert!((stats.threshold - stats.mean).abs() < 1.0,
            "limit mode threshold should equal mean, got threshold={} mean={}", stats.threshold, stats.mean);
    }

    #[test]
    fn constant_input_uses_percentile_fallback() {
        // Constant 70 dB → σ = 0 < 1.0 → normal mode uses P₉₀ = 70.0
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
