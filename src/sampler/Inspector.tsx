// The selected pad's settings: trim, level, pan, pitch, gate, reverse, mute.
import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import Wave from "./Wave";
import { AUDIO_EXTENSIONS, PAD_KEYS, type PadSettings, type PadView } from "./api";

interface Props {
  pad: PadView;
  onSettings: (s: PadSettings) => void;
  onLoadFile: (path: string) => void;
  onClear: () => void;
  onClearSteps: () => void;
  /** Plays the pad, to hear a trim. */
  onAudition: () => void;
  /** Slices the sample at its hits onto the empty pads. */
  onChop: (count: number) => void;
  freePads: number;
}

const fmtDb = (db: number) => (db <= -60 ? "off" : `${db > 0 ? "+" : ""}${db.toFixed(1)} dB`);
const fmtPan = (p: number) => (Math.abs(p) < 0.005 ? "C" : `${Math.round(Math.abs(p) * 100)}${p < 0 ? "L" : "R"}`);
const fmtPitch = (st: number) => `${st > 0 ? "+" : ""}${st} st`;
const fmtTime = (s: number) => (s < 1 ? `${Math.round(s * 1000)} ms` : `${s.toFixed(2)} s`);

export default function Inspector({ pad, onSettings, onLoadFile, onClear, onClearSteps, onAudition, onChop, freePads }: Props) {
  // Local copy so sliders move smoothly; each change is sent as it happens.
  const [s, setS] = useState(pad.settings);
  useEffect(() => setS(pad.settings), [pad.settings, pad.index]);

  function change(patch: Partial<PadSettings>) {
    const next = { ...s, ...patch };
    if (patch.start !== undefined) next.end = Math.max(next.end, next.start + 0.001);
    if (patch.end !== undefined) next.start = Math.min(next.start, next.end - 0.001);
    setS(next);
    onSettings(next);
  }

  async function pick() {
    const f = await open({ title: `Sample for pad ${pad.index + 1}`, filters: [{ name: "Audio", extensions: AUDIO_EXTENSIONS }] }).catch(() => null);
    if (typeof f === "string") onLoadFile(f);
  }

  const key = PAD_KEYS[pad.index].toUpperCase();
  const loaded = !!pad.name && !pad.missing;
  const length = pad.seconds * (s.end - s.start);

  return (
    <section className="inspector" aria-label={`Pad ${pad.index + 1} settings`}>
      <div className="pane-head">
        <h2>
          Pad {pad.index + 1} <kbd>{key}</kbd>
        </h2>
        <button className="btn small" onClick={() => void pick()}>
          {loaded ? "Replace…" : "Choose file…"}
        </button>
      </div>

      {!pad.name && (
        <div className="insp-empty">
          <strong>This pad is empty</strong>
          <span>Drag a sample onto it from the Library or Finder, or select one in the Library and press Enter.</span>
        </div>
      )}

      {pad.missing && (
        <div className="insp-empty warn-box">
          <strong>Can't find {pad.name}</strong>
          <span className="mono">{pad.path}</span>
          <span>Choose the file again, or clear the pad.</span>
        </div>
      )}

      {loaded && (
        <>
          <div className="insp-name" title={pad.path ?? ""}>
            {pad.name}
            <span className="dim mono">
              {fmtTime(length)} · {pad.channels === 1 ? "mono" : "stereo"} · {(pad.rate / 1000).toFixed(1)} kHz
            </span>
          </div>
          <Trim settings={s} peaks={pad.peaks} onChange={change} onDone={onAudition} />
          <Slider label="Start" value={s.start} min={0} max={1} step={0.001} shown={fmtTime(s.start * pad.seconds)} onChange={(v) => change({ start: v })} />
          <Slider label="End" value={s.end} min={0} max={1} step={0.001} shown={fmtTime(s.end * pad.seconds)} onChange={(v) => change({ end: v })} />
          <Slider label="Level" value={s.gainDb} min={-60} max={6} step={0.5} shown={fmtDb(s.gainDb)} onChange={(v) => change({ gainDb: v })} reset={0} />
          <Slider label="Pan" value={s.pan} min={-1} max={1} step={0.01} shown={fmtPan(s.pan)} onChange={(v) => change({ pan: v })} reset={0} />
          <Slider label="Pitch" value={s.pitch} min={-24} max={24} step={1} shown={fmtPitch(s.pitch)} onChange={(v) => change({ pitch: v })} reset={0} />
          <div className="slider">
            <label htmlFor="s-choke">Choke</label>
            <select id="s-choke" className="choke" value={s.choke} onChange={(e) => change({ choke: Number(e.target.value) })} title="Pads in the same group cut each other off, like an open and a closed hat">
              <option value={0}>None</option>
              {[1, 2, 3, 4, 5, 6, 7, 8].map((g) => (
                <option key={g} value={g}>Group {g}</option>
              ))}
            </select>
            <span />
          </div>
          <div className="toggles" role="group" aria-label="Playback">
            <Toggle on={s.gate} label="Gate" hint="Play only while held, or for one step" onClick={() => change({ gate: !s.gate })} />
            <Toggle on={s.reverse} label="Reverse" hint="Play backwards" onClick={() => change({ reverse: !s.reverse })} />
            <Toggle on={s.mute} label="Mute" hint="Leave out of the pattern (the pad still plays by hand)" onClick={() => change({ mute: !s.mute })} />
          </div>
        </>
      )}

      {pad.name && (
        <div className="insp-foot">
          {loaded && (
            <select
              className="chop-menu"
              value=""
              disabled={freePads < 2}
              aria-label="Chop this sample onto the empty pads"
              title={freePads < 2 ? "Clear some pads to chop onto them" : "Slice between start and end at the hits, onto the empty pads"}
              onChange={(e) => onChop(Number(e.target.value))}
            >
              <option value="" disabled>Chop…</option>
              {(freePads >= 4 ? [4, 8, 16].filter((n) => n <= freePads) : [freePads]).map((n) => (
                <option key={n} value={n}>
                  Into {n} slices
                </option>
              ))}
            </select>
          )}
          <button className="btn small" onClick={onClearSteps} disabled={!pad.steps.some(Boolean)}>
            Clear steps
          </button>
          <button className="btn small danger" onClick={onClear}>
            Clear pad
          </button>
        </div>
      )}
    </section>
  );
}

/** The waveform with start and end handles to drag; a click moves the nearer one. */
function Trim({ settings: s, peaks, onChange, onDone }: { settings: PadSettings; peaks: number[]; onChange: (p: Partial<PadSettings>) => void; onDone: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const which = useRef<"start" | "end" | null>(null);
  // Reversed pads draw backwards, so a handle's place on screen flips.
  const shown = (v: number) => (s.reverse ? 1 - v : v);
  const at = (clientX: number) => {
    const r = ref.current!.getBoundingClientRect();
    return shown(Math.min(1, Math.max(0, (clientX - r.left) / r.width)));
  };
  const set = (f: number) => onChange(which.current === "start" ? { start: Math.min(f, s.end - 0.001) } : { end: Math.max(f, s.start + 0.001) });
  return (
    <div
      ref={ref}
      className="trim"
      title="Drag the handles to trim; let go to hear it"
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        const f = at(e.clientX);
        which.current = Math.abs(f - s.start) <= Math.abs(f - s.end) ? "start" : "end";
        e.currentTarget.setPointerCapture(e.pointerId);
        set(f);
      }}
      onPointerMove={(e) => which.current && set(at(e.clientX))}
      onPointerUp={() => {
        if (which.current) onDone();
        which.current = null;
      }}
    >
      <Wave className="insp-wave" peaks={peaks} start={s.start} end={s.end} reverse={s.reverse} />
      <span className="handle" style={{ left: `${shown(s.start) * 100}%` }} aria-hidden="true" />
      <span className="handle end" style={{ left: `${shown(s.end) * 100}%` }} aria-hidden="true" />
    </div>
  );
}

interface SliderProps {
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  shown: string;
  onChange: (v: number) => void;
  /** Double-click the label to go back to this. */
  reset?: number;
}

function Slider({ label, value, min, max, step, shown, onChange, reset }: SliderProps) {
  const id = `s-${label.toLowerCase()}`;
  return (
    <div className="slider">
      <label htmlFor={id} onDoubleClick={() => reset !== undefined && onChange(reset)} title={reset !== undefined ? "Double-click to reset" : undefined}>
        {label}
      </label>
      <input id={id} type="range" min={min} max={max} step={step} value={value} onChange={(e) => onChange(Number(e.target.value))} />
      <output htmlFor={id} className="mono">
        {shown}
      </output>
    </div>
  );
}

function Toggle({ on, label, hint, onClick }: { on: boolean; label: string; hint: string; onClick: () => void }) {
  return (
    <button className={`toggle${on ? " on" : ""}`} aria-pressed={on} title={hint} onClick={onClick}>
      {label}
    </button>
  );
}
