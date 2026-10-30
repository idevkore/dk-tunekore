// MIDI learn in the Sampler. Two ways in:
// - Learn mode (click the MIDI light in the top bar): every control a knob can
//   learn is outlined; click one, move a knob or fader, and it's mapped. The
//   mode stays on to map more, until Done or Esc. It works by touch too.
// - Right-click a control: Learn, Learn for this pad only, or Forget.
// A mapped control carries a small mark (its controller's number in Learn
// mode). The list of mappings is in the Audio dialog (MidiMappings.tsx).
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { subscribe } from "../sampler/api";
import * as midi from "./api";
import "./midi.css";

interface MenuAt {
  x: number;
  y: number;
  target: string;
  pinned: string | null;
  label: string;
}

interface Ctx {
  view: midi.MidiView;
  mode: boolean;
  setMode: (on: boolean) => void;
  /** In Learn mode, a pad control learns for the selected pad only instead of following the selection. */
  perPad: boolean;
  selected: number;
  learn: (target: string | null) => void;
  forget: (target: string) => void;
  openMenu: (m: MenuAt) => void;
  /** "Pad 3 · Cutoff" for an id, once the targets are known. */
  name: (target: string) => string;
}

const empty: midi.MidiView = { mappings: [], learning: null, takeover: "pickup" };
const LearnContext = createContext<Ctx | null>(null);

export function useMidi(): Ctx | null {
  return useContext(LearnContext);
}

/** Where floating things go: inside an open modal dialog (the top layer), or the page. */
const layer = () => document.querySelector<HTMLElement>("dialog[open]") ?? document.body;

interface Props {
  /** The pad selected in the Sampler: what `sampler.pad.selected.*` moves. */
  selected: number;
  /** Whether the Sampler is the view on screen. Learning stops when it isn't. */
  active: boolean;
  onSay: (text: string, error?: boolean) => void;
  children: ReactNode;
}

export function MidiLearn({ selected, active, onSay, children }: Props) {
  const [view, setView] = useState<midi.MidiView>(empty);
  const [mode, setMode] = useState(false);
  const [perPad, setPerPad] = useState(false);
  const [menu, setMenu] = useState<MenuAt | null>(null);
  const [names, setNames] = useState<Map<string, string>>(new Map());
  const fail = useCallback((e: unknown) => onSay(String(e), true), [onSay]);

  useEffect(() => {
    midi.midiMappings().then(setView).catch(() => {});
    const t = midi.chosenTakeover();
    if (t) midi.midiTakeover(t).then(setView).catch(() => {});
    const offs = [
      subscribe(midi.onMappings(setView)),
      subscribe(midi.onLearned((m) => onSay(`Mapped ${m.controller} to ${m.group ? `${m.group} · ` : ""}${m.label}.`))),
    ];
    return () => offs.forEach((off) => off());
  }, [onSay]);

  useEffect(() => {
    midi.midiFocus(selected).catch(() => {});
  }, [selected]);

  const loadNames = useCallback(() => {
    midi
      .midiTargets()
      .then((list) => setNames(new Map(list.map((t) => [t.id, `${t.group} · ${t.label}`]))))
      .catch(() => {});
  }, []);

  const learn = useCallback(
    (target: string | null) => {
      if (target) loadNames();
      midi.midiLearn(target).then(setView).catch(fail);
    },
    [fail, loadNames],
  );
  const forget = useCallback((target: string) => void midi.midiForget(target).then(setView).catch(fail), [fail]);

  // Leaving the Sampler ends Learn mode and any learn waiting for a knob.
  const learning = view.learning;
  useEffect(() => {
    if (active) return;
    setMode(false);
    setMenu(null);
    if (learning) learn(null);
  }, [active, learning, learn]);

  const changeMode = useCallback(
    (on: boolean) => {
      setMode(on);
      if (on) loadNames();
      else if (learning) learn(null);
    },
    [learning, learn, loadNames],
  );

  // Learn mode: a click on a learnable control learns it instead of moving it.
  useEffect(() => {
    document.body.classList.toggle("midi-learn-on", mode);
    if (!mode) return;
    const at = (e: Event) => (e.target as Element | null)?.closest?.<HTMLElement>("[data-midi-target]") ?? null;
    const pick = (el: HTMLElement) => learn((perPad && el.dataset.midiPinned) || el.dataset.midiTarget!);
    const block = (e: Event) => {
      if (!at(e)) return;
      e.preventDefault();
      e.stopPropagation();
    };
    const click = (e: MouseEvent) => {
      const el = at(e);
      if (!el || e.button !== 0) return;
      e.preventDefault();
      e.stopPropagation();
      pick(el);
    };
    const key = (e: KeyboardEvent) => {
      const el = at(e);
      if (!el || (e.key !== "Enter" && e.key !== " ")) return;
      e.preventDefault();
      e.stopPropagation();
      pick(el);
    };
    document.addEventListener("pointerdown", block, true);
    document.addEventListener("click", click, true);
    document.addEventListener("keydown", key, true);
    return () => {
      document.body.classList.remove("midi-learn-on");
      document.removeEventListener("pointerdown", block, true);
      document.removeEventListener("click", click, true);
      document.removeEventListener("keydown", key, true);
    };
  }, [mode, perPad, learn]);

  // Esc stops a learn waiting for a knob, then Learn mode (before a dialog sees it).
  useEffect(() => {
    if (!mode && !learning) return;
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || menu) return;
      e.preventDefault();
      e.stopPropagation();
      if (learning) learn(null);
      else setMode(false);
    };
    window.addEventListener("keydown", esc, true);
    return () => window.removeEventListener("keydown", esc, true);
  }, [mode, learning, learn, menu]);

  const closeMenu = useCallback(() => setMenu(null), []);
  const name = useCallback((target: string) => names.get(target) ?? view.mappings.find((m) => m.target === target)?.label ?? target, [names, view.mappings]);

  const ctx = useMemo<Ctx>(
    () => ({ view, mode, setMode: changeMode, perPad, selected, learn, forget, openMenu: setMenu, name }),
    [view, mode, changeMode, perPad, selected, learn, forget, name],
  );

  return (
    <LearnContext.Provider value={ctx}>
      {children}
      {active && (mode || learning) && <LearnBar learning={learning} name={name} perPad={perPad} onPerPad={setPerPad} onDone={() => changeMode(false)} onCancel={() => learn(null)} mode={mode} />}
      {menu && <Menu at={menu} onClose={closeMenu} />}
    </LearnContext.Provider>
  );
}

interface LearnableProps {
  /** The id it learns: `sampler.pad.selected.cutoff`, `transport.tempo`. */
  target: string;
  /** What it is, for the menu: "Cutoff". */
  label: string;
  className?: string;
  children: ReactNode;
}

/**
 * A control a knob can learn: the element that holds it. A selected-pad
 * control can also learn for the selected pad only.
 */
export function Learnable({ target, label, className, children }: LearnableProps) {
  const c = useMidi();
  if (!c) return <div className={className}>{children}</div>;
  const pinned = midi.pinned(target, c.selected);
  const mapped = c.view.mappings.filter((m) => m.target === target || m.target === pinned);
  const waiting = !!c.view.learning && (c.view.learning === target || c.view.learning === pinned);
  const describe = (m: midi.MappingView) =>
    `${m.controller}${pinned ? (m.target === target ? ", follows the selected pad" : `, pad ${c.selected + 1} only`) : ""}`;
  const cls = ["midi-learnable", className, mapped.length ? "midi-mapped" : "", waiting ? "midi-waiting" : ""].filter(Boolean).join(" ");
  return (
    <div
      className={cls}
      data-midi-target={target}
      data-midi-pinned={pinned ?? undefined}
      tabIndex={c.mode ? 0 : undefined}
      role={c.mode ? "button" : undefined}
      aria-label={c.mode ? `Learn MIDI for ${label}${mapped.length ? ` (now ${mapped.map(describe).join("; ")})` : ""}` : undefined}
      title={c.mode ? `Click, then move a knob or fader to map ${label}` : undefined}
      onContextMenu={(e) => {
        e.preventDefault();
        c.openMenu({ x: e.clientX, y: e.clientY, target, pinned, label });
      }}
    >
      {children}
      {mapped.length > 0 && (
        <span className="midi-mark" title={`MIDI: ${mapped.map(describe).join("; ")}`} aria-label={`Mapped to MIDI ${mapped.map(describe).join("; ")}`}>
          {c.mode ? mapped.map((m) => midi.shortController(m.source).replace("CC ", "")).join(" ") : null}
        </span>
      )}
    </div>
  );
}

/** The right-click menu on a control. */
function Menu({ at, onClose }: { at: MenuAt; onClose: () => void }) {
  const c = useMidi()!;
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: at.x, top: at.y });
  useEffect(() => {
    const el = ref.current;
    if (el) {
      // Keep it on screen.
      const r = el.getBoundingClientRect();
      setPos({ left: Math.max(4, Math.min(at.x, window.innerWidth - r.width - 4)), top: Math.max(4, Math.min(at.y, window.innerHeight - r.height - 4)) });
      el.querySelector<HTMLButtonElement>("[role=menuitem]")?.focus();
    }
    const away = (e: PointerEvent) => ref.current?.contains(e.target as Node) || onClose();
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("pointerdown", away, true);
    window.addEventListener("keydown", esc, true);
    return () => {
      window.removeEventListener("pointerdown", away, true);
      window.removeEventListener("keydown", esc, true);
    };
  }, [at, onClose]);
  const pick = (f: () => void) => () => {
    onClose();
    f();
  };
  const mapped = c.view.mappings.filter((m) => m.target === at.target || m.target === at.pinned);
  const waiting = c.view.learning === at.target || (!!at.pinned && c.view.learning === at.pinned);
  return createPortal(
    <div ref={ref} className="menu midi-menu" role="menu" aria-label={`MIDI for ${at.label}`} style={pos}>
      {at.pinned ? (
        <>
          <button role="menuitem" onClick={pick(() => c.learn(at.target))}>Learn MIDI: {at.label}, selected pad</button>
          <button role="menuitem" onClick={pick(() => c.learn(at.pinned))}>Learn MIDI: {at.label}, pad {c.selected + 1} only</button>
        </>
      ) : (
        <button role="menuitem" onClick={pick(() => c.learn(at.target))}>Learn MIDI: {at.label}</button>
      )}
      {mapped.map((m) => (
        <button key={m.target} role="menuitem" onClick={pick(() => c.forget(m.target))}>
          Forget {m.controller}
          {at.pinned && <span className="dim">{m.target === at.target ? "selected pad" : `pad ${c.selected + 1}`}</span>}
        </button>
      ))}
      {waiting && <button role="menuitem" onClick={pick(() => c.learn(null))}>Stop learning</button>}
      <span className="menu-note">Then move a knob or fader on your controller.</span>
    </div>,
    layer(),
  );
}

interface BarProps {
  mode: boolean;
  learning: string | null;
  name: (target: string) => string;
  perPad: boolean;
  onPerPad: (on: boolean) => void;
  onDone: () => void;
  onCancel: () => void;
}

/** Says what Learn mode is waiting for, just above the status bar. */
function LearnBar({ mode, learning, name, perPad, onPerPad, onDone, onCancel }: BarProps) {
  return createPortal(
    <div className="midi-bar" role="status" aria-live="polite">
      <span className="midi-bar-dot" aria-hidden="true" />
      {learning ? (
        <span>
          Move a knob or fader for <strong>{name(learning)}</strong>
        </span>
      ) : (
        <span>MIDI Learn: click a control, then move a knob or fader.</span>
      )}
      {mode && (
        <label className="midi-bar-pad" title="Pad controls learn for the selected pad only, instead of following whichever pad is selected">
          <input type="checkbox" checked={perPad} onChange={(e) => onPerPad(e.target.checked)} />
          This pad only
        </label>
      )}
      {learning && (
        <button className="btn small" onClick={onCancel}>
          Cancel
        </button>
      )}
      {mode && (
        <button className="btn small primary" onClick={onDone}>
          Done
        </button>
      )}
    </div>,
    layer(),
  );
}
