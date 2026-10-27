// The step sequencer: one row per pad that has a sample, one cell per
// sixteenth. Click or drag across cells to paint; arrow keys move between
// cells and Enter or Space toggles one.
import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { AUDIO_EXTENSIONS, PAD_KEYS, PATTERN_NAMES, STEP_CHOICES, type PadView } from "./api";
import StepEditor from "./StepEditor";
import SongStrip from "./SongStrip";

interface Props {
  pads: PadView[];
  steps: number;
  /** The step playing now, or null when stopped. */
  current: number | null;
  selected: number;
  onSelect: (pad: number) => void;
  onPress: (pad: number) => void;
  onRelease: (pad: number) => void;
  onStep: (pad: number, step: number, on: boolean) => void;
  onMute: (pad: number) => void;
  /** The pad a drag is over, to highlight the add row when it's the target. */
  dropTarget: number | null;
  /** Files chosen with "Add a sound", for the empty pads in order. */
  onAddFiles: (paths: string[]) => void;
  onVelocity: (pad: number, step: number, v: number) => void;
  pattern: number;
  patternsUsed: boolean[];
  onPattern: (index: number) => void;
  onCopyPattern: (from: number, to: number) => void;
  onLength: (steps: number) => void;
  onExtras: (pad: number, step: number, chance: number, ratchet: number) => void;
  chain: number[];
  songMode: boolean;
  songLoop: boolean;
  chainPosition: number | null;
  tempo: number;
  onSong: (chain: number[], songMode: boolean, songLoop: boolean) => void;
}

export default function Steps(props: Props) {
  const { pads, steps, current, selected, onSelect, onPress, onRelease, onStep, onMute, dropTarget, onAddFiles, onVelocity } = props;
  const rows = pads.filter((p) => p.name);
  const free = pads.find((p) => !p.name);

  async function choose() {
    const picked = await open({ multiple: true, title: "Add sounds", filters: [{ name: "Audio", extensions: AUDIO_EXTENSIONS }] }).catch(() => null);
    const files = Array.isArray(picked) ? picked : typeof picked === "string" ? [picked] : [];
    if (files.length) onAddFiles(files);
  }

  // A row that turns into the next pad: click to choose files, or drop a sample on it.
  const addRow = free && (
    <div className={`add-row${dropTarget === free.index ? " over" : ""}`} data-pad={free.index}>
      <button className="add-btn" onClick={() => void choose()}>
        <span aria-hidden="true">＋</span> Add a sound
      </button>
      <span className="dim small">
        or drag one here from the Library. It goes on pad {free.index + 1} ({PAD_KEYS[free.index].toUpperCase()}).
      </span>
    </div>
  );
  // Painting: the value the first cell got, applied to every cell dragged over.
  const paint = useRef<boolean | null>(null);
  const [focus, setFocus] = useState<[number, number]>([0, 0]);
  const [editing, setEditing] = useState<{ pad: number; step: number; x: number; y: number } | null>(null);
  const gridRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const stop = () => (paint.current = null);
    window.addEventListener("pointerup", stop);
    return () => window.removeEventListener("pointerup", stop);
  }, []);

  const bank = (
    <div className="bank" role="group" aria-label="Patterns">
      <span className="bank-label">Pattern</span>
      {PATTERN_NAMES.map((n, i) => (
        <button
          key={n}
          className={`bank-btn${i === props.pattern ? " on" : ""}${props.patternsUsed[i] ? " used" : ""}`}
          aria-pressed={i === props.pattern}
          title={`Pattern ${n}${props.patternsUsed[i] ? "" : " (empty)"}. Switching while playing lands on the next step.`}
          onPointerDown={(e) => e.preventDefault()}
          onClick={() => props.onPattern(i)}
        >
          {n}
        </button>
      ))}
      <select
        className="bank-copy"
        value=""
        aria-label={`Copy pattern ${PATTERN_NAMES[props.pattern]} to another pattern`}
        title="Copy this pattern to another one, then edit the copy"
        onChange={(e) => props.onCopyPattern(props.pattern, Number(e.target.value))}
      >
        <option value="" disabled>
          Copy {PATTERN_NAMES[props.pattern]} to…
        </option>
        {PATTERN_NAMES.map((n, i) => i !== props.pattern && <option key={n} value={i}>Pattern {n}</option>)}
      </select>
      <label className="len">
        <span>Steps</span>
        <select value={steps} onChange={(e) => props.onLength(Number(e.target.value))} aria-label="Pattern length in steps">
          {STEP_CHOICES.map((n) => <option key={n} value={n}>{n}</option>)}
        </select>
      </label>
      <span className="dim small bank-hint">Right-click a step for velocity, chance and repeats</span>
    </div>
  );

  const song = (
    <SongStrip
      chain={props.chain}
      songMode={props.songMode}
      songLoop={props.songLoop}
      position={props.chainPosition}
      patternsUsed={props.patternsUsed}
      passSeconds={(steps * 15) / props.tempo}
      onChange={props.onSong}
    />
  );
  const edited = editing && pads[editing.pad];
  const editor = edited && editing && (
    <StepEditor
      key={`${editing.pad}-${editing.step}`}
      x={editing.x}
      y={editing.y}
      label={`Pad ${editing.pad + 1}, step ${editing.step + 1}`}
      velocity={edited.velocity[editing.step] ?? 1}
      chance={edited.chance[editing.step] ?? 1}
      ratchet={edited.ratchet[editing.step] ?? 1}
      onVelocity={(v) => onVelocity(editing.pad, editing.step, v)}
      onExtras={(c, r) => props.onExtras(editing.pad, editing.step, c, r)}
      onClear={() => {
        onStep(editing.pad, editing.step, false);
        setEditing(null);
      }}
      onClose={() => setEditing(null)}
    />
  );

  if (rows.length === 0) {
    return (
      <section className="steps empty-steps" aria-label="Steps">
        {bank}
        {song}
        <strong>Steps</strong>
        <span>Each sound gets a row of steps here. Click steps to make a pattern, then press Space to play it.</span>
        {addRow}
      </section>
    );
  }

  const [fr, fs] = [Math.min(focus[0], rows.length - 1), Math.min(focus[1], steps - 1)];
  const lanePad = rows.find((p) => p.index === selected) ?? rows[0];

  function moveFocus(r: number, s: number) {
    const nr = (r + rows.length) % rows.length;
    const ns = (s + steps) % steps;
    setFocus([nr, ns]);
    gridRef.current?.querySelector<HTMLButtonElement>(`[data-cell="${nr}-${ns}"]`)?.focus();
  }

  return (
    <section className="steps" aria-label="Steps">
      {bank}
      {song}
      <div
        className="grid"
        role="grid"
        aria-label={`Pattern, ${steps} steps`}
        ref={gridRef}
        style={{
          ["--rows" as string]: rows.length,
          // Cells stay near square (at most 44 px); spare width stays empty instead of stretching them.
          gridTemplateColumns: `minmax(150px, 190px) 30px repeat(${steps}, minmax(18px, 44px))`,
          gridTemplateRows: `auto repeat(${rows.length}, minmax(26px, 44px)) minmax(56px, 110px)`,
        }}
      >
        <div role="row" className="grid-row grid-head">
          <span role="columnheader" className="corner">Pad</span>
          <span role="columnheader" className="corner" title="Mute">M</span>
          {Array.from({ length: steps }, (_, s) => (
            <span role="columnheader" key={s} className={`num${s % 4 === 0 ? " beat" : ""}${current === s ? " now" : ""}`}>
              {s % 4 === 0 ? s / 4 + 1 : ""}
            </span>
          ))}
        </div>
        {rows.map((p, r) => (
          <div role="row" key={p.index} className={`grid-row${p.index === selected ? " sel" : ""}${p.settings.mute ? " muted" : ""}`}>
            <button
              role="rowheader"
              className="row-label"
              title={`Pad ${p.index + 1} (${PAD_KEYS[p.index].toUpperCase()}): click to hear and edit it`}
              onPointerDown={(e) => {
                e.preventDefault();
                onSelect(p.index);
                onPress(p.index);
              }}
              onPointerUp={() => onRelease(p.index)}
              onClick={(e) => e.detail === 0 && (onSelect(p.index), onPress(p.index), onRelease(p.index))}
            >
              <span className="row-num">{p.index + 1}</span>
              <span className="row-name">{p.name}</span>
            </button>
            <button
              className={`mute${p.settings.mute ? " on" : ""}`}
              aria-pressed={p.settings.mute}
              aria-label={`Mute pad ${p.index + 1}`}
              title="Mute in the pattern"
              onPointerDown={(e) => e.preventDefault()}
              onClick={() => onMute(p.index)}
            >
              M
            </button>
            {Array.from({ length: steps }, (_, s) => {
              const on = p.steps[s] ?? false;
              const vel = p.velocity[s] ?? 1;
              const chance = p.chance[s] ?? 1;
              const ratchet = p.ratchet[s] ?? 1;
              return (
                <button
                  key={s}
                  role="gridcell"
                  data-cell={`${r}-${s}`}
                  tabIndex={r === fr && s === fs ? 0 : -1}
                  aria-label={`Pad ${p.index + 1}, step ${s + 1}${on && vel < 1 ? `, velocity ${Math.round(vel * 100)}%` : ""}${on && chance < 1 ? `, chance ${Math.round(chance * 100)}%` : ""}${on && ratchet > 1 ? `, ${ratchet} hits` : ""}`}
                  style={on ? ({ "--vel": vel } as React.CSSProperties) : undefined}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    if (!on) onStep(p.index, s, true);
                    const r = e.currentTarget.getBoundingClientRect();
                    setEditing({ pad: p.index, step: s, x: r.left, y: r.bottom });
                  }}
                  aria-pressed={on}
                  className={`cell${on ? " on" : ""}${on && chance < 1 ? " maybe" : ""}${s % 4 === 0 ? " beat" : ""}${Math.floor(s / 4) % 2 === 1 ? " alt" : ""}${current === s ? " now" : ""}`}
                  onPointerDown={(e) => {
                    if (e.button !== 0) return;
                    e.preventDefault(); // painting shouldn't take focus from Space
                    paint.current = !on;
                    onStep(p.index, s, !on);
                    setFocus([r, s]);
                  }}
                  onPointerEnter={(e) => {
                    if (paint.current !== null && e.buttons === 1 && on !== paint.current) onStep(p.index, s, paint.current);
                  }}
                  onKeyDown={(e) => {
                    const moves: Record<string, [number, number]> = {
                      ArrowRight: [r, s + 1],
                      ArrowLeft: [r, s - 1],
                      ArrowDown: [r + 1, s],
                      ArrowUp: [r - 1, s],
                    };
                    if (e.shiftKey && (e.key === "ArrowUp" || e.key === "ArrowDown") && on) {
                      e.preventDefault();
                      onVelocity(p.index, s, Math.min(1, Math.max(0.1, vel + (e.key === "ArrowUp" ? 0.1 : -0.1))));
                    } else if (moves[e.key]) {
                      e.preventDefault();
                      moveFocus(...moves[e.key]);
                    } else if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      e.stopPropagation();
                      onStep(p.index, s, !on);
                    }
                  }}
                >
                  {on && ratchet > 1 && <span className="ratchet" aria-hidden="true">{"•".repeat(ratchet)}</span>}
                </button>
              );
            })}
          </div>
        ))}
        <VelocityLane pad={lanePad} steps={steps} current={current} onVelocity={onVelocity} />
      </div>
      {addRow}
      {editor}
    </section>
  );
}

/** Velocity bars for one pad's steps: drag up and down across them to set how hard each step plays. */
function VelocityLane({ pad, steps, current, onVelocity }: { pad: PadView; steps: number; current: number | null; onVelocity: (pad: number, step: number, v: number) => void }) {
  const dragging = useRef(false);
  const set = (x: number, y: number) => {
    const cell = document.elementFromPoint(x, y)?.closest<HTMLElement>("[data-vel]");
    if (!cell) return;
    const step = Number(cell.dataset.vel);
    if (!pad.steps[step]) return;
    const r = cell.getBoundingClientRect();
    const v = Math.round(Math.min(1, Math.max(0.05, 1 - (y - r.top) / r.height)) * 100) / 100;
    if (Math.abs(v - (pad.velocity[step] ?? 1)) >= 0.01) onVelocity(pad.index, step, v);
  };
  return (
    <div
      role="row"
      className="grid-row vel-row"
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        dragging.current = true;
        try {
          (e.target as Element).setPointerCapture(e.pointerId); // the row itself has no box to capture
        } catch {
          // a drag that leaves the window just stops
        }
        set(e.clientX, e.clientY);
      }}
      onPointerMove={(e) => dragging.current && set(e.clientX, e.clientY)}
      onPointerUp={() => (dragging.current = false)}
      onPointerCancel={() => (dragging.current = false)}
    >
      <span className="vel-label" role="rowheader" title="Drag the bars up and down to set how hard each step plays">
        Velocity
        <b>{pad.index + 1} · {pad.name}</b>
      </span>
      {Array.from({ length: steps }, (_, s) => {
        const on = pad.steps[s] ?? false;
        const v = pad.velocity[s] ?? 1;
        return (
          <span
            key={s}
            role="gridcell"
            data-vel={s}
            aria-label={on ? `Step ${s + 1}, velocity ${Math.round(v * 100)}%` : `Step ${s + 1}, off`}
            className={`vel-cell${on ? "" : " off"}${current === s ? " now" : ""}`}
          >
            {on && <i style={{ height: `${v * 100}%` }} />}
          </span>
        );
      })}
    </div>
  );
}
