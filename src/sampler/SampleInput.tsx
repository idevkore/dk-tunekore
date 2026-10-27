// "Sample from input": records from the chosen audio input and puts the take on a pad.
// The take is trimmed, saved to Music/tuneKore/Samples/Recordings and loaded by the Rust side
// (src-tauri/src/recorder.rs); the pad shows up through the usual `sampler:state` event.
import { useCallback, useEffect, useRef, useState } from "react";
import { chosenInput, onInputError, onInputLevel, samplingCancel, samplingStart, samplingStop, subscribe } from "./api";

/** The meter's range: -60 dBFS (empty) to 0 dBFS (full). */
const meterPosition = (peak: number) => (peak <= 0 ? 0 : Math.min(1, Math.max(0, (20 * Math.log10(peak) + 60) / 60)));
const fmtClock = (s: number) => `${Math.floor(s / 60)}:${(s % 60).toFixed(1).padStart(4, "0")}`;

export default function SampleInput({ pad, loaded }: { pad: number; loaded: boolean }) {
  /** The pad the running take goes to; null when not recording. */
  const [target, setTarget] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [level, setLevel] = useState(0);
  const [seconds, setSeconds] = useState(0);
  const [note, setNote] = useState<{ text: string; error: boolean } | null>(null);
  const stopping = useRef(false);

  const finish = useCallback(async () => {
    if (stopping.current) return;
    stopping.current = true;
    setBusy(true);
    try {
      const done = await samplingStop();
      setNote({ text: `Sampled ${done.seconds.toFixed(1)} s onto pad ${done.pad + 1}.`, error: false });
    } catch (e) {
      setNote({ text: String(e), error: true });
    } finally {
      setTarget(null);
      setLevel(0);
      setBusy(false);
      stopping.current = false;
    }
  }, []);

  const cancel = useCallback(async (message?: string) => {
    stopping.current = true;
    await samplingCancel().catch(() => {});
    stopping.current = false;
    setTarget(null);
    setLevel(0);
    setNote(message ? { text: message, error: true } : null);
  }, []);

  useEffect(() => {
    if (target === null) return;
    const offs = [
      subscribe(
        onInputLevel((l) => {
          setLevel((prev) => Math.max(l.peak, prev * 0.85));
          setSeconds(l.seconds);
          if (l.full) void finish(); // the 10-minute cap: keep what we have
        }),
      ),
      subscribe(onInputError((m) => void cancel(`${m}. The take was dropped.`))),
    ];
    return () => offs.forEach((off) => off());
  }, [target, finish, cancel]);

  async function start() {
    setBusy(true);
    setNote(null);
    try {
      await samplingStart(pad, chosenInput());
      setSeconds(0);
      setTarget(pad);
    } catch (e) {
      setNote({ text: String(e), error: true });
    } finally {
      setBusy(false);
    }
  }

  const recording = target !== null;
  return (
    <div className="sample-input">
      {recording ? (
        <>
          <div className="sample-row">
            <span className="rec-dot" aria-hidden="true" />
            <span className="mono" role="timer" aria-label="Time recorded">{fmtClock(seconds)}</span>
            <div className="rec-meter" role="meter" aria-label="Input level" aria-valuemin={0} aria-valuemax={1} aria-valuenow={meterPosition(level)}>
              <div style={{ transform: `scaleX(${meterPosition(level)})` }} />
            </div>
          </div>
          <div className="sample-row">
            <span className="dim small" role="status">Sampling onto pad {target + 1}</span>
            <span style={{ flex: 1 }} />
            <button className="btn small" onClick={() => void cancel()} disabled={busy}>Cancel</button>
            <button className="btn small primary" onClick={() => void finish()} disabled={busy} autoFocus>Stop</button>
          </div>
        </>
      ) : (
        <div className="sample-row">
          <button className="btn small" onClick={() => void start()} disabled={busy} title="Record from the audio input chosen in Audio settings onto this pad">
            {loaded ? "Replace from input" : "Sample from input"}
          </button>
          <span className="dim small">microphone or audio interface</span>
        </div>
      )}
      {note && (
        <div className={`small${note.error ? " warn" : " dim"}`} role={note.error ? "alert" : "status"} style={note.error ? { color: "var(--warn)" } : undefined}>
          {note.text}
        </div>
      )}
    </div>
  );
}
