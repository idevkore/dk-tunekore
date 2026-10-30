// The MIDI mappings, in the Audio dialog: what each knob moves, Forget,
// "Map a control…" for controls with nothing on screen (a track's volume,
// another pad's cutoff), and what a knob does when it and its control disagree.
import { useEffect, useMemo, useState } from "react";
import * as midi from "./api";
import { useMidi } from "./learn";

const TAKEOVER: { mode: midi.Takeover; label: string; help: string }[] = [
  { mode: "pickup", label: "Pick up", help: "Nothing changes until the knob reaches the control's value, then it follows. No jumps." },
  { mode: "scale", label: "Catch up", help: "The value moves at once, toward the end the knob is heading for, until the two meet. No jumps." },
  { mode: "jump", label: "Jump", help: "The value jumps to wherever the knob is." },
];

export default function MidiMappings({ onError }: { onError: (e: unknown) => void }) {
  const c = useMidi();
  const [targets, setTargets] = useState<midi.TargetInfo[]>([]);
  useEffect(() => {
    midi.midiTargets().then(setTargets).catch(() => {});
  }, []);
  const groups = useMemo(() => {
    const g = new Map<string, midi.TargetInfo[]>();
    targets.forEach((t) => g.set(t.group, [...(g.get(t.group) ?? []), t]));
    return [...g.entries()];
  }, [targets]);
  if (!c) return null;
  const { view } = c;
  const sorted = [...view.mappings].sort((a, b) => (a.group + a.label).localeCompare(b.group + b.label));
  const takeover = TAKEOVER.find((t) => t.mode === view.takeover) ?? TAKEOVER[0];

  return (
    <div className="field midi-maps">
      <span>MIDI mappings</span>
      {sorted.length ? (
        <ul className="midi-map-list" aria-label="MIDI mappings">
          {sorted.map((m) => (
            <li key={m.target} className={m.available ? undefined : "gone"}>
              <span className="midi-map-name" title={m.target}>
                {m.available ? (
                  <>
                    <span className="dim">{m.group} · </span>
                    {m.label}
                  </>
                ) : (
                  <>
                    {m.label} <span className="dim">(not in this project now)</span>
                  </>
                )}
              </span>
              <span className="mono midi-map-cc">{m.controller}</span>
              <button className="btn small" onClick={() => c.forget(m.target)} aria-label={`Forget ${m.controller} on ${m.group} ${m.label}`}>
                Forget
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <span className="dim small">None yet. Click MIDI in the top bar, click a control, then move a knob; or right-click a control.</span>
      )}
      <div className="field-row">
        {view.learning ? (
          <>
            <span className="midi-map-wait">
              Move a knob or fader for <strong>{c.name(view.learning)}</strong>
            </span>
            <button className="btn small" onClick={() => c.learn(null)}>
              Cancel
            </button>
          </>
        ) : (
          <select
            className="midi-map-add"
            value=""
            aria-label="Map a control to a knob or fader"
            onChange={(e) => e.target.value && c.learn(e.target.value)}
          >
            <option value="" disabled>
              Map a control…
            </option>
            {groups.map(([group, list]) => (
              <optgroup key={group} label={group}>
                {list.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.label}
                  </option>
                ))}
              </optgroup>
            ))}
          </select>
        )}
      </div>
      <label className="field-inline">
        <span>When a knob and its control disagree</span>
        <select
          value={view.takeover}
          onChange={(e) => {
            const mode = e.target.value as midi.Takeover;
            midi.chooseTakeover(mode);
            midi.midiTakeover(mode).catch(onError);
          }}
        >
          {TAKEOVER.map((t) => (
            <option key={t.mode} value={t.mode}>
              {t.label}
            </option>
          ))}
        </select>
      </label>
      <p className="field-help">
        {takeover.help} Mappings are saved with the project. One knob moves one control; learning a knob again moves it to the new control.
      </p>
    </div>
  );
}
