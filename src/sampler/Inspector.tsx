// The selected pad's settings: trim, level, pan, pitch, gate, reverse, mute.
// Every slider can learn a MIDI knob (src/midi/learn.tsx) for the selected
// pad, or for this pad only.
import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import Wave from "./Wave";
import SampleInput from "./SampleInput";
import { AUDIO_EXTENSIONS, PAD_KEYS, type PadSettings, type PadView } from "./api";
import { Learnable } from "../midi/learn";
import { padLearn } from "../midi/api";

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
const fmtHz = (hz: number) => (hz >= 1000 ? `${(hz / 1000).toFixed(hz >= 10000 ? 0 : 1)} kHz` : `${Math.round(hz)} Hz`);
/** Whether the Shape tab differs from the defaults, to mark the tab. */
const shaped = (s: PadSettings) =>
  s.attack > 0 || s.decay > 0 || s.filter !== 0 || Math.abs(s.release - 0.004) > 0.0005 || s.fenvAmount !== 0 || s.velCutoff !== 0 || s.velTime !== 0;
const fmtTime = (s: number) => (s < 1 ? `${Math.round(s * 1000)} ms` : `${s.toFixed(2)} s`);
const fmtOct = (o: number) => (Math.abs(o) < 0.05 ? "off" : `${o > 0 ? "+" : "−"}${Math.abs(o).toFixed(1)} oct`);
const fmtPct = (v: number) => (Math.abs(v) < 0.005 ? "off" : `${v > 0 ? "+" : "−"}${Math.round(Math.abs(v) * 100)}%`);

export default function Inspector({ pad, onSettings, onLoadFile, onClear, onClearSteps, onAudition, onChop, freePads }: Props) {
  // Local copy so sliders move smoothly; each change is sent as it happens.
  const [s, setS] = useState(pad.settings);
  const [tab, setTab] = useState<"sample" | "shape">("sample");
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
          <div className="tabs" role="tablist" aria-label="Pad settings">
            {(["sample", "shape"] as const).map((t) => (
              <button
                key={t}
                role="tab"
                id={`tab-${t}`}
                aria-selected={tab === t}
                aria-controls={`panel-${t}`}
                tabIndex={tab === t ? 0 : -1}
                className={tab === t ? "on" : ""}
                onClick={() => setTab(t)}
                onKeyDown={(e) => {
                  if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
                    const next = tab === "sample" ? "shape" : "sample";
                    setTab(next);
                    document.getElementById(`tab-${next}`)?.focus();
                  }
                }}
              >
                {t === "sample" ? "Sample" : "Shape"}
                {t === "shape" && shaped(s) && <span className="tab-dot" aria-label="(changed)" />}
              </button>
            ))}
          </div>
          {tab === "shape" && (
            <div role="tabpanel" id="panel-shape" aria-labelledby="tab-shape" className="panel">
              <h3 className="panel-sub">Amp</h3>
              <Curve {...padLearn("attack")} label="Attack" value={s.attack} max={2} shown={fmtTime(s.attack)} onChange={(v) => change({ attack: v })} reset={0} />
              <Curve {...padLearn("decay")} label="Decay" value={s.decay} max={5} shown={s.decay === 0 ? "off" : fmtTime(s.decay)} onChange={(v) => change({ decay: v })} reset={0} />
              <Slider {...padLearn("sustain")} label="Sustain" value={s.sustain} min={0} max={1} step={0.01} shown={`${Math.round(s.sustain * 100)}%`} onChange={(v) => change({ sustain: v })} reset={1} />
              <Curve {...padLearn("release")} label="Release" value={s.release} min={0.001} max={5} shown={fmtTime(s.release)} onChange={(v) => change({ release: v })} reset={0.004} />
              <h3 className="panel-sub">Filter</h3>
              <div className="slider">
                <span className="lbl">Filter</span>
                <div className="seg mini" role="radiogroup" aria-label="Filter type">
                  {["Off", "Low", "High", "Band"].map((n, i) => (
                    <label key={n} className={s.filter === i ? "on" : ""} title={["No filter", "Low-pass: keeps the lows", "High-pass: keeps the highs", "Band-pass: keeps a band around the cutoff"][i]}>
                      <input type="radio" name="filter" checked={s.filter === i} onChange={() => change({ filter: i })} />
                      {n}
                    </label>
                  ))}
                </div>
                <span />
              </div>
              <Log {...padLearn("cutoff")} label="Cutoff" value={s.cutoff} min={20} max={20000} shown={fmtHz(s.cutoff)} onChange={(v) => change({ cutoff: v })} disabled={s.filter === 0} reset={20000} />
              <Slider {...padLearn("resonance")} label="Reso" value={s.resonance} min={0} max={1} step={0.01} shown={`${Math.round(s.resonance * 100)}%`} onChange={(v) => change({ resonance: v })} reset={0} />
              <div className={`panel${s.filter === 0 ? " dimmed" : ""}`} title={s.filter === 0 ? "Turn the filter on to use its envelope" : undefined}>
                <h3 className="panel-sub">Filter envelope</h3>
                <Slider {...padLearn("fenvAmount")} id="fenv-amount" label="Amount" value={s.fenvAmount} min={-8} max={8} step={0.1} shown={fmtOct(s.fenvAmount)} onChange={(v) => change({ fenvAmount: v })} reset={0} />
                <Curve {...padLearn("fenvAttack")} id="fenv-attack" label="Attack" value={s.fenvAttack} max={2} shown={fmtTime(s.fenvAttack)} onChange={(v) => change({ fenvAttack: v })} reset={0} />
                <Curve {...padLearn("fenvDecay")} id="fenv-decay" label="Decay" value={s.fenvDecay} max={5} shown={s.fenvDecay === 0 ? "off" : fmtTime(s.fenvDecay)} onChange={(v) => change({ fenvDecay: v })} reset={0.3} />
                <Slider {...padLearn("fenvSustain")} id="fenv-sustain" label="Sustain" value={s.fenvSustain} min={0} max={1} step={0.01} shown={`${Math.round(s.fenvSustain * 100)}%`} onChange={(v) => change({ fenvSustain: v })} reset={0} />
                <Curve {...padLearn("fenvRelease")} id="fenv-release" label="Release" value={s.fenvRelease} min={0.001} max={5} shown={fmtTime(s.fenvRelease)} onChange={(v) => change({ fenvRelease: v })} reset={0.1} />
              </div>
              <h3 className="panel-sub">Velocity</h3>
              <div className={s.filter === 0 ? "dimmed" : ""}>
                <Slider {...padLearn("velCutoff")} id="vel-cutoff" label="Cutoff" value={s.velCutoff} min={-8} max={8} step={0.1} shown={fmtOct(s.velCutoff)} onChange={(v) => change({ velCutoff: v })} reset={0} />
              </div>
              <Slider {...padLearn("velTime")} id="vel-time" label="Time" value={s.velTime} min={-1} max={1} step={0.01} shown={fmtPct(s.velTime)} onChange={(v) => change({ velTime: v })} reset={0} />
              <p className="dim small hint">
                Decay falls to the sustain level while the sound plays; release is how long it fades when let go (gate pads). The filter envelope moves the cutoff on every hit, up or down by its amount. Velocity Cutoff above zero makes soft hits darker; Time above zero makes their attack and decay shorter, below zero longer. Double-click a label to reset it.
              </p>
            </div>
          )}
          {tab === "sample" && (
          <div role="tabpanel" id="panel-sample" aria-labelledby="tab-sample" className="panel">
          <Slider {...padLearn("start")} label="Start" value={s.start} min={0} max={1} step={0.001} shown={fmtTime(s.start * pad.seconds)} onChange={(v) => change({ start: v })} />
          <Slider {...padLearn("end")} label="End" value={s.end} min={0} max={1} step={0.001} shown={fmtTime(s.end * pad.seconds)} onChange={(v) => change({ end: v })} />
          <Slider {...padLearn("gainDb")} label="Level" value={s.gainDb} min={-60} max={6} step={0.5} shown={fmtDb(s.gainDb)} onChange={(v) => change({ gainDb: v })} reset={0} />
          <Slider {...padLearn("pan")} label="Pan" value={s.pan} min={-1} max={1} step={0.01} shown={fmtPan(s.pan)} onChange={(v) => change({ pan: v })} reset={0} />
          <Slider {...padLearn("pitch")} label="Pitch" value={s.pitch} min={-24} max={24} step={1} shown={fmtPitch(s.pitch)} onChange={(v) => change({ pitch: v })} reset={0} />
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
          </div>
          )}
        </>
      )}

      <SampleInput pad={pad.index} loaded={loaded} />

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
  /** For the label's `for`; defaults to the label. Needed when two sliders share a label. */
  id?: string;
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  shown: string;
  onChange: (v: number) => void;
  /** Double-click the label to go back to this. */
  reset?: number;
  /** The MIDI learn id for it (src/midi/api.ts), and its name there. */
  target?: string;
  learnAs?: string;
}

function Slider({ id: given, label, value, min, max, step, shown, onChange, reset, target, learnAs }: SliderProps) {
  const id = `s-${given ?? label.toLowerCase()}`;
  const row = (
    <>
      <label htmlFor={id} onDoubleClick={() => reset !== undefined && onChange(reset)} title={reset !== undefined ? "Double-click to reset" : undefined}>
        {label}
      </label>
      <input id={id} type="range" min={min} max={max} step={step} value={value} onChange={(e) => onChange(Number(e.target.value))} />
      <output htmlFor={id} className="mono">
        {shown}
      </output>
    </>
  );
  return target ? (
    <Learnable target={target} label={learnAs ?? label} className="slider">
      {row}
    </Learnable>
  ) : (
    <div className="slider">{row}</div>
  );
}

type Learn = { target?: string; learnAs?: string };

/** A time slider with a squared scale, so short times get most of the travel. */
function Curve({ id, label, value, min = 0, max, shown, onChange, reset, target, learnAs }: { id?: string; label: string; value: number; min?: number; max: number; shown: string; onChange: (v: number) => void; reset?: number } & Learn) {
  const pos = Math.sqrt(Math.max(0, (value - min) / (max - min)));
  return (
    <Slider target={target} learnAs={learnAs} id={id} label={label} value={pos} min={0} max={1} step={0.001} shown={shown} onChange={(p) => onChange(min + p * p * (max - min))} reset={reset === undefined ? undefined : Math.sqrt(Math.max(0, (reset - min) / (max - min)))} />
  );
}

/** A frequency slider on a log scale. */
function Log({ label, value, min, max, shown, onChange, disabled, reset, target, learnAs }: { label: string; value: number; min: number; max: number; shown: string; onChange: (v: number) => void; disabled?: boolean; reset?: number } & Learn) {
  const pos = Math.log(value / min) / Math.log(max / min);
  return (
    <div className={disabled ? "dimmed" : ""}>
      <Slider target={target} learnAs={learnAs} label={label} value={pos} min={0} max={1} step={0.001} shown={shown} onChange={(p) => onChange(min * Math.pow(max / min, p))} reset={reset === undefined ? undefined : Math.log(reset / min) / Math.log(max / min)} />
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
