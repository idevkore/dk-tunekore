// A small editor for one step, opened by right-clicking it: how hard it
// plays, how likely it is to play, and how many times it repeats.
import { useEffect, useRef, useState } from "react";

interface Props {
  /** Where to open, in viewport pixels (the cell's bottom-left). */
  x: number;
  y: number;
  label: string;
  velocity: number;
  chance: number;
  ratchet: number;
  onVelocity: (v: number) => void;
  onExtras: (chance: number, ratchet: number) => void;
  onClear: () => void;
  onClose: () => void;
}

export default function StepEditor(p: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [vel, setVel] = useState(p.velocity);
  const [chance, setChance] = useState(p.chance);
  const [ratchet, setRatchet] = useState(p.ratchet);

  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    ref.current?.querySelector<HTMLInputElement>("input")?.focus();
    const away = (e: PointerEvent) => ref.current?.contains(e.target as Node) || p.onClose();
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        p.onClose();
      }
    };
    window.addEventListener("pointerdown", away, true);
    window.addEventListener("keydown", key, true);
    return () => {
      window.removeEventListener("pointerdown", away, true);
      window.removeEventListener("keydown", key, true);
      opener?.focus();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Keep it on screen.
  const left = Math.min(p.x, window.innerWidth - 250);
  const top = p.y + 210 > window.innerHeight ? p.y - 250 : p.y + 6;

  return (
    <div ref={ref} className="step-editor" role="dialog" aria-label={p.label} style={{ left, top }}>
      <strong>{p.label}</strong>
      <label className="se-row">
        <span>Velocity</span>
        <input
          type="range"
          min={0.05}
          max={1}
          step={0.01}
          value={vel}
          onChange={(e) => {
            setVel(Number(e.target.value));
            p.onVelocity(Number(e.target.value));
          }}
        />
        <output className="mono">{Math.round(vel * 100)}%</output>
      </label>
      <label className="se-row">
        <span>Chance</span>
        <input
          type="range"
          min={0}
          max={1}
          step={0.05}
          value={chance}
          onChange={(e) => {
            setChance(Number(e.target.value));
            p.onExtras(Number(e.target.value), ratchet);
          }}
        />
        <output className="mono">{Math.round(chance * 100)}%</output>
      </label>
      <div className="se-row">
        <span>Repeat</span>
        <div className="seg mini" role="radiogroup" aria-label="Hits in this step">
          {[1, 2, 3, 4].map((n) => (
            <label key={n} className={ratchet === n ? "on" : ""} title={n === 1 ? "Plays once" : `Plays ${n} times within the step`}>
              <input
                type="radio"
                name="ratchet"
                checked={ratchet === n}
                onChange={() => {
                  setRatchet(n);
                  p.onExtras(chance, n);
                }}
              />
              {n}×
            </label>
          ))}
        </div>
      </div>
      <div className="se-foot">
        <button className="btn small" onClick={p.onClear}>
          Turn off
        </button>
        <button className="btn small primary" onClick={p.onClose}>
          Done
        </button>
      </div>
    </div>
  );
}
