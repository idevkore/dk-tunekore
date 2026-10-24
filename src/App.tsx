import { useCallback, useEffect, useState } from "react";
import logo from "../brand/tunekore-logo-color.svg";
import {
  engineVersion,
  listOutputs,
  onMeter,
  startTone,
  stopTone,
  type OutputDevice,
} from "./engine";
import { openAnalyzer } from "./analyzer/api";

const DEFAULT_HZ = 440;

/** Peak 0..1 to a 0..1 bar position on a -60..0 dB scale. */
function meterPosition(peak: number): number {
  if (peak <= 0) return 0;
  const db = 20 * Math.log10(peak);
  return Math.min(1, Math.max(0, (db + 60) / 60));
}

export default function App() {
  const [version, setVersion] = useState("");
  const [devices, setDevices] = useState<OutputDevice[]>([]);
  const [deviceId, setDeviceId] = useState<string | null>(null);
  const [frequency, setFrequency] = useState(DEFAULT_HZ);
  const [playing, setPlaying] = useState(false);
  const [level, setLevel] = useState(0);
  const [error, setError] = useState("");

  const refresh = useCallback(async () => {
    try {
      const outs = await listOutputs();
      setDevices(outs);
      setDeviceId((cur) =>
        cur !== null && outs.some((d) => d.id === cur)
          ? cur
          : (outs.find((d) => d.isDefault) ?? outs[0])?.id ?? null,
      );
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    engineVersion().then(setVersion).catch((e) => setError(String(e)));
    void refresh();
    let off: (() => void) | undefined;
    let gone = false;
    onMeter((peak) => setLevel((prev) => Math.max(peak, prev * 0.8)))
      .then((u) => (gone ? u() : (off = u)))
      .catch(() => {});
    return () => {
      gone = true;
      off?.();
    };
  }, [refresh]);

  // Let the bar fall back to zero after the last meter event.
  useEffect(() => {
    if (level === 0) return;
    const t = setTimeout(() => setLevel((l) => (l < 0.0005 ? 0 : l * 0.8)), 60);
    return () => clearTimeout(t);
  }, [level]);

  async function toggle() {
    try {
      if (playing) {
        await stopTone();
        setPlaying(false);
      } else {
        await startTone(deviceId, frequency);
        setPlaying(true);
      }
      setError("");
    } catch (e) {
      setPlaying(false);
      setError(String(e));
    }
  }

  async function changeFrequency(hz: number) {
    setFrequency(hz);
    if (!playing) return;
    try {
      await startTone(deviceId, hz);
    } catch (e) {
      setError(String(e));
    }
  }

  async function changeDevice(id: string) {
    setDeviceId(id);
    if (!playing) return;
    try {
      await startTone(id, frequency);
    } catch (e) {
      setPlaying(false);
      setError(String(e));
    }
  }

  const pos = meterPosition(level);

  return (
    <main className="app">
      <header className="top">
        <img src={logo} alt="tuneKore" className="logo" />
        <span className="version">engine {version || "…"}</span>
      </header>

      <section className="panel">
        <h2>Analyzer</h2>
        <div className="row">
          <p className="lead">Split a song into vocals, drums, bass and other.</p>
          <button className="play" onClick={() => openAnalyzer().catch((e) => setError(String(e)))}>
            Open the Analyzer
          </button>
        </div>
      </section>

      <section className="panel">
        <h2>Audio output</h2>
        <div className="row">
          <select
            aria-label="Output device"
            value={deviceId ?? ""}
            onChange={(e) => void changeDevice(e.target.value)}
            disabled={devices.length === 0}
          >
            {devices.length === 0 && <option value="">No output devices</option>}
            {devices.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}
                {d.isDefault ? " (default)" : ""} · {d.channels} ch
              </option>
            ))}
          </select>
          <button className="ghost" onClick={() => void refresh()}>
            Rescan
          </button>
        </div>
      </section>

      <section className="panel">
        <h2>Test tone</h2>
        <div className="row">
          <button
            className={playing ? "stop" : "play"}
            onClick={() => void toggle()}
            disabled={devices.length === 0}
          >
            {playing ? "Stop" : "Play"}
          </button>
          <label className="freq">
            <span>{frequency} Hz</span>
            <input
              type="range"
              min={100}
              max={2000}
              step={1}
              value={frequency}
              onChange={(e) => void changeFrequency(Number(e.target.value))}
            />
          </label>
        </div>
        <div
          className="meter"
          role="meter"
          aria-label="Output level"
          aria-valuemin={-60}
          aria-valuemax={0}
          aria-valuenow={level > 0 ? Math.round(20 * Math.log10(level)) : -60}
        >
          <div className="meter-fill" style={{ transform: `scaleX(${pos})` }} />
        </div>
        <div className="meter-scale">
          <span>-60</span>
          <span>-40</span>
          <span>-20</span>
          <span>0 dB</span>
        </div>
      </section>

      {error && <p className="error">{error}</p>}
    </main>
  );
}
