// The step sequencer: one row per pad that has a sample, one cell per
// sixteenth. Click or drag across cells to paint; arrow keys move between
// cells and Enter or Space toggles one.
import { useEffect, useRef, useState } from "react";
import { PAD_KEYS, type PadView } from "./api";

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
}

export default function Steps({ pads, steps, current, selected, onSelect, onPress, onRelease, onStep, onMute }: Props) {
  const rows = pads.filter((p) => p.name);
  // Painting: the value the first cell got, applied to every cell dragged over.
  const paint = useRef<boolean | null>(null);
  const [focus, setFocus] = useState<[number, number]>([0, 0]);
  const gridRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const stop = () => (paint.current = null);
    window.addEventListener("pointerup", stop);
    return () => window.removeEventListener("pointerup", stop);
  }, []);

  if (rows.length === 0) {
    return (
      <section className="steps empty-steps" aria-label="Steps">
        <strong>Steps</strong>
        <span>Put samples on pads, then click here to make a pattern. Press Space to play it.</span>
      </section>
    );
  }

  const [fr, fs] = [Math.min(focus[0], rows.length - 1), Math.min(focus[1], steps - 1)];

  function moveFocus(r: number, s: number) {
    const nr = (r + rows.length) % rows.length;
    const ns = (s + steps) % steps;
    setFocus([nr, ns]);
    gridRef.current?.querySelector<HTMLButtonElement>(`[data-cell="${nr}-${ns}"]`)?.focus();
  }

  return (
    <section className="steps" aria-label="Steps">
      <div
        className="grid"
        role="grid"
        aria-label={`Pattern, ${steps} steps`}
        ref={gridRef}
        style={{ gridTemplateColumns: `minmax(150px, 190px) 30px repeat(${steps}, minmax(18px, 1fr))` }}
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
              return (
                <button
                  key={s}
                  role="gridcell"
                  data-cell={`${r}-${s}`}
                  tabIndex={r === fr && s === fs ? 0 : -1}
                  aria-label={`Pad ${p.index + 1}, step ${s + 1}`}
                  aria-pressed={on}
                  className={`cell${on ? " on" : ""}${s % 4 === 0 ? " beat" : ""}${Math.floor(s / 4) % 2 === 1 ? " alt" : ""}${current === s ? " now" : ""}`}
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
                    if (moves[e.key]) {
                      e.preventDefault();
                      moveFocus(...moves[e.key]);
                    } else if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      e.stopPropagation();
                      onStep(p.index, s, !on);
                    }
                  }}
                />
              );
            })}
          </div>
        ))}
      </div>
    </section>
  );
}
