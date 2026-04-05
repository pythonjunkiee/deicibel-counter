import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow, PhysicalPosition } from "@tauri-apps/api/window";
import {
  Check,
  GripHorizontal,
  Mic,
  MicOff,
  RefreshCw,
  RotateCcw,
  Settings,
  Volume2,
  VolumeX,
  X,
  Zap,
} from "lucide-react";

// ─── Constants ────────────────────────────────────────────────────────────────

const SCALE_MIN = 0;
const SCALE_MAX = 100;
const DEFAULT_THRESHOLD = 75;
const EMA_ALPHA = 0.35;
const ALERT_LATCH_MS = 750;
const PEAK_HOLD_MS = 2000;
const CALIBRATION_DURATION_MS = 10_000;

// ─── Types ────────────────────────────────────────────────────────────────────

interface DbPayload {
  db: number;
}

interface CalibrationResult {
  threshold: number;
  mean: number;
  std_dev: number;
  min: number;
  max: number;
  mode: "normal" | "limit";
}

type CalibPhase = "idle" | "recording" | "confirm";

// ─── Helpers ─────────────────────────────────────────────────────────────────

function barColor(db: number, threshold: number): string {
  if (db > threshold)       return "#f87171";
  if (db > threshold * 0.8) return "#fbbf24";
  return "#34d399";
}

function barGradientClass(db: number, threshold: number): string {
  if (db > threshold)       return "from-red-600 via-red-500 to-orange-400";
  if (db > threshold * 0.8) return "from-amber-500 via-yellow-400 to-amber-300";
  return                           "from-emerald-500 via-teal-400 to-cyan-400";
}

const clamp = (v: number, lo: number, hi: number) =>
  Math.min(hi, Math.max(lo, v));

const pct = (v: number) =>
  clamp(((v - SCALE_MIN) / (SCALE_MAX - SCALE_MIN)) * 100, 0, 100);

// ─── Component ────────────────────────────────────────────────────────────────

export default function App() {
  // ── State ──────────────────────────────────────────────────────────────────

  const [displayDb,    setDisplayDb]    = useState(0);
  const [threshold,    setThreshold]    = useState(() => {
    const saved = localStorage.getItem("db-threshold");
    return saved !== null ? Number(saved) : DEFAULT_THRESHOLD;
  });
  const [showSettings, setShowSettings] = useState(false);
  const [isAlerting,   setIsAlerting]   = useState(false);
  const [micError,     setMicError]     = useState(false);
  const [isMuted,      setIsMuted]      = useState(false);
  const [displayPeak,  setDisplayPeak]  = useState(0);

  // Calibration
  const [calibPhase,    setCalibPhase]    = useState<CalibPhase>("idle");
  const [calibProgress, setCalibProgress] = useState(0);
  const [pendingResult, setPendingResult] = useState<CalibrationResult | null>(null);
  const [pendingThreshold, setPendingThreshold] = useState(DEFAULT_THRESHOLD);

  // Refs
  const smoothed         = useRef(0);
  const alertTimer       = useRef<ReturnType<typeof setTimeout> | null>(null);
  const shakeKey         = useRef(0);
  const peakDb           = useRef(0);
  const peakTimer        = useRef<ReturnType<typeof setTimeout> | null>(null);
  const calibIntervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const calibStartRef    = useRef(0);
  const positionSaveRef  = useRef<ReturnType<typeof setTimeout> | null>(null);

  // ── Persist threshold ──────────────────────────────────────────────────────
  useEffect(() => {
    localStorage.setItem("db-threshold", String(threshold));
  }, [threshold]);

  // ── Restore + persist window position ─────────────────────────────────────
  useEffect(() => {
    const win = getCurrentWindow();
    const sx = localStorage.getItem("win-x");
    const sy = localStorage.getItem("win-y");
    if (sx !== null && sy !== null) {
      win.setPosition(new PhysicalPosition(Number(sx), Number(sy))).catch(() => {});
    }
    let unlistenMove: UnlistenFn | undefined;
    win.listen<{ x: number; y: number }>("tauri://move", (e) => {
      if (positionSaveRef.current) clearTimeout(positionSaveRef.current);
      positionSaveRef.current = setTimeout(() => {
        localStorage.setItem("win-x", String(e.payload.x));
        localStorage.setItem("win-y", String(e.payload.y));
      }, 300);
    }).then((fn) => { unlistenMove = fn; });
    return () => {
      unlistenMove?.();
      if (positionSaveRef.current) clearTimeout(positionSaveRef.current);
    };
  }, []);

  // ── EMA smoothing + alert latch + peak hold ────────────────────────────────
  const processDb = useCallback(
    (raw: number) => {
      if (!Number.isFinite(raw) || raw < 0 || raw > 120) return;
      smoothed.current = EMA_ALPHA * raw + (1 - EMA_ALPHA) * smoothed.current;
      const val = Math.round(smoothed.current);
      setDisplayDb(val);

      if (val > peakDb.current) {
        peakDb.current = val;
        setDisplayPeak(val);
        if (peakTimer.current) clearTimeout(peakTimer.current);
        peakTimer.current = setTimeout(() => {
          peakDb.current = 0;
          setDisplayPeak(0);
          peakTimer.current = null;
        }, PEAK_HOLD_MS);
      }

      if (val > threshold) {
        if (!alertTimer.current) shakeKey.current += 1;
        setIsAlerting(true);
        if (alertTimer.current) clearTimeout(alertTimer.current);
        alertTimer.current = setTimeout(() => {
          setIsAlerting(false);
          alertTimer.current = null;
        }, ALERT_LATCH_MS);
      }
    },
    [threshold]
  );

  // ── Tauri event subscriptions ──────────────────────────────────────────────
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let mounted = true;

    listen<DbPayload>("db-level", (event) => {
      if (!mounted) return;
      processDb(event.payload.db);
    })
      .then((fn) => { unlisten = fn; })
      .catch(() => setMicError(true));

    listen<null>("mic-error", () => {
      if (mounted) setMicError(true);
    }).then((fn) => {
      const original = unlisten;
      unlisten = () => { original?.(); fn(); };
    });

    return () => {
      mounted = false;
      unlisten?.();
      if (alertTimer.current) clearTimeout(alertTimer.current);
      if (peakTimer.current)  clearTimeout(peakTimer.current);
    };
  }, [processDb]);

  // ── Calibration result listener ────────────────────────────────────────────
  useEffect(() => {
    let unlistenCalib: UnlistenFn | undefined;

    listen<CalibrationResult>("calibration-result", (event) => {
      const result = event.payload;
      if (calibIntervalRef.current) {
        clearInterval(calibIntervalRef.current);
        calibIntervalRef.current = null;
      }
      setCalibProgress(100);

      // Short pause so bar visually completes, then show confirmation
      setTimeout(() => {
        setPendingResult(result);
        setPendingThreshold(Math.round(result.threshold));
        setCalibPhase("confirm");
        setCalibProgress(0);
      }, 400);
    }).then((fn) => { unlistenCalib = fn; });

    return () => { unlistenCalib?.(); };
  }, []);

  // ── Action handlers ────────────────────────────────────────────────────────

  const handleMuteToggle = async () => {
    const newMuted = await invoke<boolean>("toggle_mute");
    setIsMuted(newMuted);
    if (newMuted) {
      smoothed.current = 0;
      setDisplayDb(0);
      peakDb.current = 0;
      setDisplayPeak(0);
    }
  };

  const handleRetry = async () => {
    setMicError(false);
    await invoke("retry_audio");
  };

  const startCalib = async (mode: "normal" | "limit") => {
    if (calibPhase === "recording" || micError) return;
    setCalibPhase("recording");
    setCalibProgress(0);
    setPendingResult(null);
    calibStartRef.current = Date.now();
    calibIntervalRef.current = setInterval(() => {
      const elapsed = Date.now() - calibStartRef.current;
      setCalibProgress(Math.min((elapsed / CALIBRATION_DURATION_MS) * 100, 99));
    }, 100);
    await invoke("start_calibration", { mode });
  };

  const handleAcceptCalib = () => {
    if (pendingResult) setThreshold(pendingThreshold);
    setCalibPhase("idle");
    setPendingResult(null);
  };

  const handleRedoCalib = async (mode: "normal" | "limit") => {
    setCalibPhase("idle");
    setPendingResult(null);
    await startCalib(mode);
  };

  const closeWindow = () => getCurrentWindow().close();

  // ── Derived display values ─────────────────────────────────────────────────
  const isCalibrating = calibPhase === "recording";
  const isConfirming  = calibPhase === "confirm";

  const barPct       = pct(displayDb);
  const thresholdPct = pct(threshold);
  const peakPct      = pct(displayPeak);
  const accentColor  = barColor(displayDb, threshold);
  const gradClass    = barGradientClass(displayDb, threshold);

  const secondsLeft = Math.max(
    1,
    Math.ceil(CALIBRATION_DURATION_MS / 1000 - (calibProgress / 100) * (CALIBRATION_DURATION_MS / 1000))
  );

  // ── Render ─────────────────────────────────────────────────────────────────
  return (
    <div data-tauri-drag-region className="w-full h-full">
      <div
        className={[
          "w-full select-none overflow-hidden rounded-2xl border font-mono text-sm shadow-2xl",
          isAlerting && !isCalibrating && !isConfirming
            ? "border-red-500/60 animate-alert-glow"
            : isCalibrating
            ? "border-cyan-500/30"
            : isConfirming
            ? "border-cyan-400/40"
            : "border-white/10",
        ].join(" ")}
        style={{
          background: isAlerting && !isCalibrating && !isConfirming
            ? "rgba(20,8,8,0.97)"
            : isCalibrating || isConfirming
            ? "rgba(4,20,25,0.97)"
            : "rgba(10,12,20,0.97)",
          boxShadow: isAlerting && !isCalibrating
            ? "0 0 24px 4px rgba(239,68,68,0.25), 0 8px 32px rgba(0,0,0,0.7)"
            : "0 8px 32px rgba(0,0,0,0.7)",
        }}
      >
        {/* ── Title bar ──────────────────────────────────────────────────── */}
        <div
          data-tauri-drag-region
          className="flex items-center justify-between px-3 py-2 border-b border-white/6 cursor-grab active:cursor-grabbing"
          style={{ background: "rgba(255,255,255,0.03)" }}
        >
          <div data-tauri-drag-region className="flex items-center gap-1.5">
            <GripHorizontal size={11} className="text-gray-600" data-tauri-drag-region />
            {micError ? (
              <MicOff size={11} className="text-red-500" />
            ) : isMuted ? (
              <MicOff size={11} className="text-amber-400" />
            ) : isCalibrating ? (
              <Mic size={11} className="text-cyan-400 animate-pulse" />
            ) : (
              <Mic size={11} className={isAlerting ? "text-red-400" : "text-cyan-400"} />
            )}
            <span className="text-[10px] tracking-[0.2em] uppercase text-gray-500">
              {isCalibrating ? "Calibrating" : isConfirming ? "Review result" : "dB Meter"}
            </span>
          </div>

          <div className="flex items-center gap-0.5">
            {!micError && !isCalibrating && !isConfirming && (
              <button
                onClick={handleMuteToggle}
                aria-label={isMuted ? "Unmute" : "Mute"}
                className={[
                  "p-1.5 rounded-lg transition-colors",
                  isMuted ? "bg-amber-500/20 text-amber-400" : "text-gray-600 hover:text-gray-300",
                ].join(" ")}
              >
                {isMuted ? <VolumeX size={11} /> : <Volume2 size={11} />}
              </button>
            )}
            {!isCalibrating && !isConfirming && (
              <button
                onClick={() => setShowSettings((s) => !s)}
                aria-label="Toggle settings"
                className={[
                  "p-1.5 rounded-lg transition-colors",
                  showSettings ? "bg-cyan-500/20 text-cyan-400" : "text-gray-600 hover:text-gray-300",
                ].join(" ")}
              >
                <Settings size={11} />
              </button>
            )}
            <button
              onClick={closeWindow}
              aria-label="Close"
              className="p-1.5 rounded-lg text-gray-600 hover:text-red-400 transition-colors"
            >
              <X size={11} />
            </button>
          </div>
        </div>

        {/* ── Main area ──────────────────────────────────────────────────── */}
        <div className="px-4 pt-3 pb-2.5 space-y-2.5">

          {/* ── Recording phase ────────────────────────────────────── */}
          {isCalibrating && (
            <div className="space-y-2.5">
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-1.5">
                  <Mic size={12} className="text-cyan-400 animate-pulse" />
                  <span className="text-[10px] tracking-[0.18em] uppercase text-cyan-400">
                    Listening...
                  </span>
                </div>
                <span className="text-[22px] font-bold tabular-nums" style={{ color: "#22d3ee" }}>
                  {secondsLeft}
                  <span className="text-[9px] font-normal text-gray-600 ml-0.5">s</span>
                </span>
              </div>
              <div className="h-2 rounded-full overflow-hidden" style={{ background: "rgba(255,255,255,0.06)" }}>
                <div
                  className="h-full rounded-full transition-[width] duration-100 ease-linear"
                  style={{ width: `${calibProgress}%`, background: "linear-gradient(to right,#164e63,#22d3ee,#2dd4bf)" }}
                />
              </div>
              {/* Live dB reading during calibration */}
              <div className="flex items-center justify-center gap-1.5 py-0.5">
                <span className="text-[11px] text-gray-600">Live:</span>
                <span className="text-[13px] font-bold tabular-nums" style={{ color: accentColor }}>
                  {displayDb} dB
                </span>
              </div>
            </div>
          )}

          {/* ── Confirmation phase ─────────────────────────────────── */}
          {isConfirming && pendingResult && (
            <div className="space-y-2">
              {/* Detected range card */}
              <div
                className="rounded-xl px-3 py-2 space-y-1.5"
                style={{ background: "rgba(34,211,238,0.06)", border: "1px solid rgba(34,211,238,0.15)" }}
              >
                <p className="text-[9px] uppercase tracking-[0.2em] text-cyan-600">Detected range</p>
                <div className="flex items-baseline justify-between">
                  <div className="text-center">
                    <div className="text-[10px] text-gray-600">Min</div>
                    <div className="text-[15px] font-bold tabular-nums text-gray-300">
                      {Math.round(pendingResult.min)}
                    </div>
                  </div>
                  <div className="text-center">
                    <div className="text-[10px] text-gray-600">Avg</div>
                    <div className="text-[15px] font-bold tabular-nums" style={{ color: "#22d3ee" }}>
                      {Math.round(pendingResult.mean)}
                    </div>
                  </div>
                  <div className="text-center">
                    <div className="text-[10px] text-gray-600">Max</div>
                    <div className="text-[15px] font-bold tabular-nums text-gray-300">
                      {Math.round(pendingResult.max)}
                    </div>
                  </div>
                  <div className="text-center">
                    <div className="text-[10px] text-gray-600">Suggested</div>
                    <div className="text-[15px] font-bold tabular-nums text-amber-400">
                      {Math.round(pendingResult.threshold)}
                    </div>
                  </div>
                </div>
              </div>

              {/* Editable threshold */}
              <div className="space-y-1">
                <div className="flex items-center justify-between">
                  <span className="text-[9px] uppercase tracking-[0.15em] text-gray-600">
                    Warn me at
                  </span>
                  <span className="text-[11px] font-bold tabular-nums text-amber-400">
                    {pendingThreshold} dB
                  </span>
                </div>
                <input
                  type="range"
                  min={SCALE_MIN}
                  max={SCALE_MAX}
                  step={1}
                  value={pendingThreshold}
                  onChange={(e) => setPendingThreshold(Number(e.target.value))}
                  className="w-full"
                  aria-label="Adjust calibrated threshold"
                />
              </div>

              {/* Accept / Redo */}
              <div className="flex gap-2 pt-0.5">
                <button
                  onClick={handleAcceptCalib}
                  className="flex-1 flex items-center justify-center gap-1 py-1.5 rounded-lg text-[9px] tracking-[0.15em] uppercase font-semibold transition-colors"
                  style={{ background: "rgba(52,211,153,0.12)", border: "1px solid rgba(52,211,153,0.35)", color: "#34d399" }}
                >
                  <Check size={9} />
                  Apply
                </button>
                <button
                  onClick={() => handleRedoCalib(pendingResult.mode)}
                  className="flex-1 flex items-center justify-center gap-1 py-1.5 rounded-lg text-[9px] tracking-[0.15em] uppercase font-semibold transition-colors"
                  style={{ background: "rgba(255,255,255,0.04)", border: "1px solid rgba(255,255,255,0.1)", color: "#6b7280" }}
                >
                  <RotateCcw size={9} />
                  Redo
                </button>
              </div>
            </div>
          )}

          {/* ── Normal HUD ─────────────────────────────────────────── */}
          {!isCalibrating && !isConfirming && (
            <>
              <div className="flex items-baseline justify-between">
                <div className="flex items-baseline gap-1.5">
                  <span
                    key={shakeKey.current}
                    className={[
                      "text-4xl font-bold tabular-nums tracking-tight leading-none",
                      isAlerting ? "animate-shake" : micError ? "text-gray-600" : "",
                    ].join(" ")}
                    style={{ color: micError ? undefined : isMuted ? "rgba(245,158,11,0.5)" : accentColor }}
                  >
                    {micError ? "--" : displayDb}
                  </span>
                  <span className="text-xs font-normal text-gray-500 tracking-widest">dB</span>
                </div>

                {micError ? (
                  <button
                    onClick={handleRetry}
                    className="flex items-center gap-1 text-[9px] tracking-widest uppercase px-2 py-0.5 rounded-full font-semibold text-cyan-500 hover:text-cyan-300 transition-colors"
                    style={{ border: "1px solid rgba(8,145,178,0.4)", background: "rgba(8,145,178,0.1)" }}
                  >
                    <RefreshCw size={8} /> RETRY
                  </button>
                ) : (
                  <span
                    className="text-[9px] tracking-widest uppercase px-2 py-0.5 rounded-full font-semibold"
                    style={{
                      border: isMuted ? "1px solid rgba(245,158,11,0.4)" : isAlerting ? "1px solid rgba(239,68,68,0.4)" : displayDb > threshold * 0.8 ? "1px solid rgba(245,158,11,0.4)" : "1px solid rgba(52,211,153,0.4)",
                      background: isMuted ? "rgba(245,158,11,0.1)" : isAlerting ? "rgba(239,68,68,0.12)" : displayDb > threshold * 0.8 ? "rgba(245,158,11,0.1)" : "rgba(52,211,153,0.1)",
                      color: isMuted ? "#fbbf24" : isAlerting ? "#f87171" : displayDb > threshold * 0.8 ? "#fbbf24" : "#34d399",
                    }}
                  >
                    {isMuted ? "MUTED" : isAlerting ? "TOO LOUD" : displayDb > threshold * 0.8 ? "LOUD" : "OK"}
                  </span>
                )}
              </div>

              {/* Meter bar */}
              <div className="relative h-2.5 rounded-full overflow-visible" style={{ background: "rgba(255,255,255,0.06)" }}>
                <div
                  className={`absolute inset-y-0 left-0 rounded-full bg-gradient-to-r ${gradClass} transition-[width] duration-75 ease-out`}
                  style={{ width: `${barPct}%` }}
                />
                {displayPeak > 0 && (
                  <div
                    className="absolute top-1/2 -translate-y-1/2 w-[2px] h-[14px] rounded-full z-20"
                    style={{ left: `calc(${peakPct}% - 1px)`, background: "rgba(251,191,36,0.7)" }}
                    title={`Peak: ${displayPeak} dB`}
                  />
                )}
                <div
                  className="absolute top-1/2 -translate-y-1/2 w-[2px] h-[18px] rounded-full z-10"
                  style={{ left: `calc(${thresholdPct}% - 1px)`, background: "rgba(255,255,255,0.5)" }}
                  title={`Alert at ${threshold} dB`}
                />
              </div>

              <div className="flex justify-between text-[9px] tracking-wider" style={{ color: "rgba(75,85,99,1)" }}>
                <span>{SCALE_MIN}</span>
                <span style={{ color: isAlerting ? "#dc2626" : "rgba(107,114,128,1)" }}>
                  ▲&nbsp;{threshold}&nbsp;dB
                </span>
                <span>{SCALE_MAX}</span>
              </div>
            </>
          )}
        </div>

        {/* ── Settings panel ──────────────────────────────────────────────── */}
        <div
          className={[
            "overflow-hidden transition-all duration-300 ease-in-out",
            showSettings && !isCalibrating && !isConfirming
              ? "max-h-72 opacity-100"
              : "max-h-0 opacity-0 pointer-events-none",
          ].join(" ")}
        >
          <div className="px-4 pt-2 pb-3.5 border-t space-y-3" style={{ borderColor: "rgba(255,255,255,0.06)" }}>

            {/* ── Manual threshold slider ──────────────────────────── */}
            <div className="space-y-1.5">
              <div className="flex items-center justify-between">
                <span className="text-[9px] uppercase tracking-[0.2em] text-gray-600">Alert threshold</span>
                <span className="text-[11px] font-bold text-cyan-400 tabular-nums">{threshold}&nbsp;dB</span>
              </div>
              <input
                type="range"
                min={SCALE_MIN}
                max={SCALE_MAX}
                step={1}
                value={threshold}
                onChange={(e) => setThreshold(Number(e.target.value))}
                className="w-full"
                aria-label="Alert threshold in decibels"
              />
              <p className="text-[9px] text-gray-700 leading-tight">
                Drag to manually set your warning level.
              </p>
            </div>

            {/* ── Calibration buttons ─────────────────────────────── */}
            <div className="space-y-1.5 pt-1 border-t" style={{ borderColor: "rgba(255,255,255,0.06)" }}>
              <p className="text-[9px] uppercase tracking-[0.2em] text-gray-600">Auto-calibrate (10 s)</p>

              {/* Normal mode */}
              <button
                onClick={() => startCalib("normal")}
                disabled={micError}
                className="w-full flex items-center justify-center gap-1.5 py-1.5 rounded-lg text-[9px] tracking-[0.12em] uppercase font-semibold transition-colors"
                style={{
                  border: micError ? "1px solid rgba(55,65,81,1)" : "1px solid rgba(8,145,178,0.4)",
                  background: micError ? "rgba(17,24,39,0.3)" : "rgba(8,145,178,0.08)",
                  color: micError ? "rgba(75,85,99,1)" : "#22d3ee",
                  cursor: micError ? "not-allowed" : "pointer",
                }}
              >
                <Mic size={9} />
                Speak normally → set threshold above
              </button>

              {/* Limit mode */}
              <button
                onClick={() => startCalib("limit")}
                disabled={micError}
                className="w-full flex items-center justify-center gap-1.5 py-1.5 rounded-lg text-[9px] tracking-[0.12em] uppercase font-semibold transition-colors"
                style={{
                  border: micError ? "1px solid rgba(55,65,81,1)" : "1px solid rgba(245,158,11,0.4)",
                  background: micError ? "rgba(17,24,39,0.3)" : "rgba(245,158,11,0.08)",
                  color: micError ? "rgba(75,85,99,1)" : "#fbbf24",
                  cursor: micError ? "not-allowed" : "pointer",
                }}
              >
                <Zap size={9} />
                Speak at your limit → set threshold there
              </button>

              <p className="text-[9px] text-gray-700 leading-snug">
                <span className="text-cyan-700">Normal</span> — speak as you normally would; warns when you go louder than usual.{" "}
                <span className="text-amber-700">Limit</span> — speak at the exact volume you want to trigger a warning.
              </p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
