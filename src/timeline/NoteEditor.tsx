// The note editor (idevkore/dk-tunekore#38): the selected MIDI clip's notes,
// in a panel under the tracks where a clip's inspector goes.
//
// Down the left a keyboard plays each key through the track's instrument;
// along the top the ruler counts bars of the clip's own time, and its brace
// is the clip's loop (or, for a clip that doesn't loop, the part it plays):
// drag its ends or its middle. Notes outside it don't play and are shaded.
// Along the bottom each note's velocity is a stem to drag.
//
// Notes: drag to move (⌥ copies), drag either end to resize, double-click
// (or click with Draw on, B) to add one, double-click one (or click it with
// Draw on) to delete it, drag on empty space to select with a box (⇧ adds).
// Everything snaps to the editor's grid unless ⌘ (Ctrl elsewhere) is held.
// Keys while the editor has focus: ⌫ deletes the selected notes, ⌘A selects
// them all, the arrows move them (⇧ by an octave or a bar), Q quantizes them
// (all the notes when none is selected), ⌘D duplicates them, B switches
// Draw, Esc lets go of the selection. Space and ⌘Z stay the Timeline's.
//
// Each gesture goes to the Rust side as one undoable edit of the clip with
// all its notes; the editor shows it at once and until the new state lands.
// Edits go one at a time, each once the last has landed, so they land in the
// order they were made, and what shows stays until the last of them has.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import * as api from "./api";
import { COLOURS, type MidiClipIn, type MidiClipView, type MidiNote, type TrackView } from "./api";
import { ignoresGrid, position } from "./edits";
import { clipTimeAt, DEFAULT_VELOCITY, isLoopedMidi, keyName, midiEnd, MIN_BEATS, nextNoteId, quantize, setLoopedMidi, setRegion, sortNotes, toMidiInput } from "./midi";
import { beatX, BRACE_H, drawNotes, gridBottom, KEY_W, keyY, NOTE_EDGE, RULER_H, VEL_H, xBeat, yKey, type NoteView } from "./noteDraw";
import { useTheme } from "../theme/theme";

type Grid = "bar" | "beat" | "eighth" | "sixteenth" | "thirtysecond" | "triplet" | "off";
const GRIDS: { id: Grid; label: string }[] = [
  { id: "bar", label: "Bar" },
  { id: "beat", label: "1/4" },
  { id: "eighth", label: "1/8" },
  { id: "sixteenth", label: "1/16" },
  { id: "triplet", label: "1/16T" },
  { id: "thirtysecond", label: "1/32" },
  { id: "off", label: "Off" },
];
const gridOf = (g: Grid, bar: number) => ({ bar, beat: 1, eighth: 0.5, sixteenth: 0.25, triplet: 1 / 6, thirtysecond: 0.125, off: 0 })[g];

const MIN_ROW = 6;
const MAX_ROW = 26;
const MIN_ZOOM = 4;
const MAX_ZOOM = 1600;
const MIN_PANEL = 200;
const MAX_PANEL = 720;
/** Movement before a press becomes a drag. */
const DRAG_PX = 3;
const mod = navigator.platform.toLowerCase().includes("mac") ? "⌘" : "Ctrl+";

type Zone = "move" | "start" | "end";
type Op =
  | { kind: Zone; x0: number; y0: number; anchor: MidiNote; base: MidiNote[]; ids: Set<number>; moved: boolean; erase: boolean }
  | { kind: "draw"; note: MidiNote; base: MidiNote[]; moved: boolean }
  | { kind: "marquee"; x0: number; y0: number; keep: Set<number> }
  | { kind: "vel"; anchor: MidiNote; base: MidiNote[]; ids: Set<number> }
  | { kind: "regionStart" | "regionEnd" | "regionMove"; x0: number; clip: MidiClipView }
  | { kind: "key"; key: number };

type Props = {
  clip: MidiClipView;
  track: TrackView;
  signature: [number, number];
  /** The Timeline's playhead now, and whether it moves. */
  playhead: () => number;
  playing: boolean;
  /** Sends a change to the clip as one undoable edit; a refusal drops the preview. */
  onEdit: (clip: MidiClipIn, key: string) => Promise<unknown>;
  onDelete: () => void;
  /** Esc with no note selected: back to the Timeline. */
  onClose: () => void;
};

const clamp = (x: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, x));

/** Counts gestures for their undo keys; outlives the editor, so a remount never repeats a key. */
let gestures = 0;

/** Keeps the pointer's events coming here through a drag. */
function capture(e: React.PointerEvent<Element>) {
  try {
    e.currentTarget.setPointerCapture(e.pointerId);
  } catch {
    // a synthetic or already-released pointer
  }
}

export default function NoteEditor({ clip, track, signature, playhead, playing, onEdit, onDelete, onClose }: Props) {
  const bar = (signature[0] * 4) / signature[1];
  const [grid, setGrid] = useState<Grid>("sixteenth");
  const step = gridOf(grid, bar);
  const [drawMode, setDrawMode] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  // What shows ahead of the Rust side (a gesture, or edits on their way), for the clip it belongs to.
  const [shown, setShown] = useState<{ id: number; notes: MidiNote[] } | null>(null);
  const [shownClip, setShownClip] = useState<MidiClipView | null>(null);
  const [marquee, setMarquee] = useState<{ x0: number; y0: number; x1: number; y1: number } | null>(null);
  const [held, setHeld] = useState<Set<number>>(new Set());
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [panel, setPanel] = useState(300);
  const [view, setView] = useState<NoteView>({ scrollX: 0, zoomX: 48, scrollY: 0, rowH: 12 });
  const [name, setName] = useState(clip.name);
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const head = useRef<HTMLDivElement>(null);
  const nameBox = useRef<HTMLInputElement>(null);
  const op = useRef<Op | null>(null);
  /** Edits sent and not landed yet, and the last of them, which the next waits for. */
  const pending = useRef(0);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  /** A gesture let go: back to what was last sent (or the Rust side's state when nothing is on its way). */
  const unshape = useCallback(() => {
    setShown(null);
    setShownClip(null);
  }, []);
  const lastLength = useRef(0.25);
  const sounding = useRef<number | null>(null);
  const resize = useRef<{ y0: number; h0: number } | null>(null);
  const theme = useTheme();
  /** Per clip, the last notes and the last clip sent and not landed yet. */
  const sentNotes = useRef(new Map<number, MidiNote[]>());
  const sentClip = useRef(new Map<number, MidiClipView>());
  // Another clip's (the selection moved while its edits were on their way) never shows here; this
  // clip's own edits still on their way do, after a switch away and back too.
  const notes = shown?.id === clip.id ? shown.notes : sentNotes.current.get(clip.id) ?? clip.notes;
  const c = shownClip?.id === clip.id ? shownClip : sentClip.current.get(clip.id) ?? clip;
  const colour = COLOURS[track.colour % COLOURS.length];
  const live = useRef({ view, size, clip, notes });
  live.current = { view, size, clip, notes };

  // New state from the Rust side: the edit landed, the preview goes.
  useEffect(() => {
    if (!op.current && pending.current === 0) {
      setShown(null);
      setShownClip(null);
    }
    // Not while the name is being typed.
    if (document.activeElement !== nameBox.current) setName(clip.name);
  }, [clip]);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setSize({ w: el.clientWidth, h: el.clientHeight }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const clampView = useCallback((v: NoteView, h = live.current.size.h): NoteView => {
    const gridH = Math.max(1, gridBottom(h) - RULER_H);
    const rowH = clamp(v.rowH, MIN_ROW, MAX_ROW);
    return { rowH, zoomX: clamp(v.zoomX, MIN_ZOOM, MAX_ZOOM), scrollX: Math.max(-8 / v.zoomX, v.scrollX), scrollY: clamp(v.scrollY, 0, Math.max(0, 128 * rowH - gridH)) };
  }, []);

  /** Shows the whole clip, and its notes (or the keys round C3) in the middle. */
  const fit = useCallback(() => {
    const { size, clip } = live.current;
    if (size.w <= KEY_W) return;
    const ns = clip.notes;
    const from = Math.min(clip.offset, ...ns.map((n) => n.start));
    const to = Math.max(clip.offset + clip.content, ...ns.map((n) => n.start + n.length));
    const zoomX = (size.w - KEY_W - 24) / Math.max(to - from, 1);
    const lo = ns.length ? Math.min(...ns.map((n) => n.key)) : 60;
    const hi = ns.length ? Math.max(...ns.map((n) => n.key)) : 72;
    const gridH = gridBottom(size.h) - RULER_H;
    setView((v) => clampView({ ...v, zoomX, scrollX: from - 8 / zoomX, scrollY: (127 - (lo + hi) / 2) * v.rowH - gridH / 2 }));
  }, [clampView]);

  // A new clip, or the panel's first size: fit it.
  const fitted = useRef<number | null>(null);
  useEffect(() => {
    if (size.w > 0 && fitted.current !== clip.id) {
      fitted.current = clip.id;
      setSelected(new Set());
      fit();
    }
  }, [clip.id, size.w, fit]);

  useLayoutEffect(() => {
    const ctx = canvas.current?.getContext("2d");
    if (!ctx || size.w <= 0 || size.h <= 0) return;
    drawNotes(ctx, { width: size.w, height: size.h, dpr: window.devicePixelRatio || 1, view, clip: c, notes, selected, colour, barBeats: bar, step, marquee, held });
  }, [size, view, c, notes, selected, colour, bar, step, marquee, held, theme]);

  // The playhead, where the clip is playing, every frame while it plays.
  useEffect(() => {
    const place = () => {
      const el = head.current;
      if (!el) return;
      const { view, size, clip } = live.current;
      const u = clipTimeAt(clip, playhead());
      const x = u === null ? -1 : beatX(view, u);
      el.style.transform = `translateX(${Math.round(x)}px)`;
      el.style.visibility = u !== null && x >= KEY_W && x <= size.w ? "visible" : "hidden";
    };
    place();
    if (!playing) return;
    let raf = 0;
    const frame = () => {
      place();
      raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
  }, [playing, view, size, clip, playhead]);

  // ---- Sound: the key under the pointer, through the track's instrument ----

  const sound = useCallback(
    (key: number | null, velocity = DEFAULT_VELOCITY) => {
      const was = sounding.current;
      if (was === key) return;
      if (was !== null) void api.previewNote(track.id, was, 0, false).catch(() => {});
      if (key !== null) void api.previewNote(track.id, key, velocity, true).catch(() => {});
      sounding.current = key;
      setHeld(key === null ? new Set() : new Set([key]));
    },
    [track.id],
  );
  useEffect(
    () => () => {
      if (sounding.current !== null) void api.previewOff(track.id).catch(() => {});
      sounding.current = null;
    },
    [track.id],
  );

  // ---- Edits ----

  /** Sends an edit once the ones before it have landed. Each names its clip, so it lands on that
   *  clip whatever is selected by then. Once the last has landed (or was refused), the Rust side's
   *  state shows as it is. */
  const send = useCallback(
    (edit: MidiClipIn, key: string, refused?: () => void) => {
      pending.current += 1;
      const next = queue.current.catch(() => {}).then(() => onEdit(edit, key));
      queue.current = next;
      next
        .catch(() => refused?.())
        .finally(() => {
          pending.current -= 1;
          if (pending.current === 0) {
            sentNotes.current.clear();
            sentClip.current.clear();
            if (!op.current) {
              setShown(null);
              setShownClip(null);
            }
          }
        });
    },
    [onEdit],
  );

  const commit = useCallback(
    (next: MidiNote[], what: string, merge = false) => {
      const key = merge ? `notes:${clip.id}:${what}` : `notes:${clip.id}:${what}:${++gestures}`;
      sentNotes.current.set(clip.id, next);
      setShown({ id: clip.id, notes: next });
      // The clip as shown: a loop change still on its way isn't undone by this.
      send({ ...toMidiInput(c, track.id), notes: sortNotes(next) }, key);
    },
    [clip.id, c, track.id, send],
  );

  const commitClip = useCallback(
    (next: MidiClipView, what: string) => {
      sentClip.current.set(clip.id, next);
      setShownClip(next);
      send(toMidiInput(next, track.id), `${what}:${clip.id}`);
    },
    [clip.id, track.id, send],
  );

  const snapTo = (beat: number, free: boolean) => (free || step <= 0 ? beat : Math.round(beat / step) * step);
  const snapDown = (beat: number, free: boolean) => (free || step <= 0 ? beat : Math.floor(beat / step + 1e-9) * step);
  const minLength = step > 0 ? Math.min(step, 0.25) : MIN_BEATS;

  const addNote = (beat: number, key: number, free: boolean): MidiNote => {
    const start = Math.max(0, snapDown(beat, free));
    // From the notes shown, so a second note drawn before the first has landed gets an id of its own.
    return { id: nextNoteId(notes), key, start, length: Math.max(MIN_BEATS, lastLength.current), velocity: DEFAULT_VELOCITY };
  };

  const removeNotes = (ids: Set<number>) => {
    if (!ids.size) return;
    commit(
      notes.filter((n) => !ids.has(n.id)),
      "delete",
    );
    setSelected(new Set());
  };

  const quantizeNotes = () => {
    const next = quantize(notes, selected, step > 0 ? step : 0.25);
    if (next.some((n, i) => n.start !== notes[i].start)) commit(next, "quantize");
  };

  const duplicate = () => {
    const picked = notes.filter((n) => selected.has(n.id));
    if (!picked.length) return;
    const from = Math.min(...picked.map((n) => n.start));
    const to = Math.max(...picked.map((n) => n.start + n.length));
    const d = step > 0 ? Math.ceil((to - from) / step - 1e-9) * step : to - from;
    let id = nextNoteId(notes);
    const copies = picked.map((n) => ({ ...n, id: id++, start: n.start + d }));
    commit([...notes, ...copies], "duplicate");
    setSelected(new Set(copies.map((n) => n.id)));
  };

  /** Moves the selected notes by `beats` and `keys`, as far as they can go. */
  const nudge = (beats: number, keys: number) => {
    const picked = notes.filter((n) => selected.has(n.id));
    if (!picked.length) return;
    const db = Math.max(beats, -Math.min(...picked.map((n) => n.start)));
    const dk = clamp(keys, -Math.min(...picked.map((n) => n.key)), 127 - Math.max(...picked.map((n) => n.key)));
    if (db === 0 && dk === 0) return;
    commit(
      notes.map((n) => (selected.has(n.id) ? { ...n, start: n.start + db, key: n.key + dk } : n)),
      "nudge",
      true,
    );
  };

  // ---- Pointer ----

  const local = (e: { clientX: number; clientY: number }) => {
    const r = canvas.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  const area = (x: number, y: number) => {
    const bottom = gridBottom(size.h);
    if (y < RULER_H) return x >= KEY_W ? "ruler" : "corner";
    if (y >= bottom) return x >= KEY_W ? "vel" : "corner";
    return x < KEY_W ? "keys" : "grid";
  };

  const noteAt = (x: number, y: number): { note: MidiNote; zone: Zone } | null => {
    const key = yKey(view, y);
    for (let i = notes.length - 1; i >= 0; i--) {
      const n = notes[i];
      if (n.key !== key) continue;
      const x0 = beatX(view, n.start);
      const x1 = beatX(view, n.start + n.length);
      if (x < x0 - 1 || x > x1 + 1) continue;
      const w = x1 - x0;
      const zone: Zone = x1 - x <= Math.min(NOTE_EDGE, w / 3) ? "end" : w > 14 && x - x0 <= Math.min(NOTE_EDGE - 1, w / 4) ? "start" : "move";
      return { note: n, zone };
    }
    return null;
  };

  /** The note whose velocity stem is under (x, y): the nearest, the one whose top is nearest among a chord's. */
  const stemAt = (x: number, y: number): MidiNote | null => {
    const room = VEL_H - 10;
    let best: MidiNote | null = null;
    let score = Infinity;
    for (const n of notes) {
      const dx = Math.abs(beatX(view, n.start) - x);
      if (dx > 6) continue;
      const top = size.h - 2 - (n.velocity / 127) * room;
      const s = dx * 10 + Math.abs(top - y) + (selected.has(n.id) ? -5 : 0);
      if (s < score) {
        score = s;
        best = n;
      }
    }
    return best;
  };

  const velocityAt = (y: number) => clamp(Math.round(((size.h - 2 - y) / (VEL_H - 10)) * 127), 1, 127);

  const onDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (e.button !== 0) return;
    box.current?.parentElement?.focus({ preventScroll: true });
    const { x, y } = local(e);
    const free = ignoresGrid(e);
    const where = area(x, y);
    if (where === "keys") {
      capture(e);
      const key = yKey(view, y);
      op.current = { kind: "key", key };
      sound(key);
      return;
    }
    if (where === "ruler") {
      if (y > BRACE_H + 2) return;
      const ax = beatX(view, c.offset);
      const bx = beatX(view, c.offset + c.content);
      const kind = Math.abs(x - ax) <= NOTE_EDGE ? "regionStart" : Math.abs(x - bx) <= NOTE_EDGE ? "regionEnd" : x > ax && x < bx ? "regionMove" : null;
      if (!kind) return;
      capture(e);
      op.current = { kind, x0: x, clip: c };
      return;
    }
    if (where === "vel") {
      const n = stemAt(x, y);
      if (!n) return;
      capture(e);
      const ids = selected.has(n.id) ? selected : new Set([n.id]);
      if (!selected.has(n.id)) setSelected(ids);
      op.current = { kind: "vel", anchor: n, base: notes, ids };
      return;
    }
    if (where !== "grid") return;
    capture(e);
    const hit = noteAt(x, y);
    if (hit) {
      const n = hit.note;
      let sel: Set<number>;
      if (e.shiftKey) {
        sel = new Set(selected);
        if (sel.has(n.id)) sel.delete(n.id);
        else sel.add(n.id);
      } else sel = selected.has(n.id) ? selected : new Set([n.id]);
      setSelected(sel);
      if (!sel.has(n.id)) return;
      op.current = { kind: hit.zone, x0: x, y0: y, anchor: n, base: notes, ids: sel, moved: false, erase: drawMode && !e.shiftKey };
      sound(n.key, n.velocity);
      return;
    }
    if (drawMode) {
      const note = addNote(xBeat(view, x), yKey(view, y), free);
      op.current = { kind: "draw", note, base: notes, moved: false };
      setShown({ id: clip.id, notes: [...notes, note] });
      setSelected(new Set([note.id]));
      sound(note.key);
      return;
    }
    const keep = e.shiftKey ? new Set(selected) : new Set<number>();
    if (!e.shiftKey) setSelected(new Set());
    op.current = { kind: "marquee", x0: x, y0: y, keep };
  };

  /** The notes as the gesture leaves them with the pointer at (x, y). With `listen`, a moved note sounds at its new key. */
  const shape = (o: Op, x: number, y: number, free: boolean, copy: boolean, listen = false) => {
    const beat = xBeat(view, x);
    switch (o.kind) {
      case "move": {
        const picked = o.base.filter((n) => o.ids.has(n.id));
        const d = Math.max(snapTo(o.anchor.start + (beat - xBeat(view, o.x0)), free) - o.anchor.start, -Math.min(...picked.map((n) => n.start)));
        const dk = clamp(yKey(view, y) - yKey(view, o.y0), -Math.min(...picked.map((n) => n.key)), 127 - Math.max(...picked.map((n) => n.key)));
        if (listen) sound(o.anchor.key + dk, o.anchor.velocity);
        if (copy) {
          let id = nextNoteId(o.base);
          return [...o.base, ...picked.map((n) => ({ ...n, id: id++, start: n.start + d, key: n.key + dk }))];
        }
        return o.base.map((n) => (o.ids.has(n.id) ? { ...n, start: n.start + d, key: n.key + dk } : n));
      }
      case "end": {
        const end = o.anchor.start + o.anchor.length;
        const d = snapTo(end + (beat - xBeat(view, o.x0)), free) - end;
        return o.base.map((n) => (o.ids.has(n.id) ? { ...n, length: Math.max(free ? MIN_BEATS : minLength, n.length + d) } : n));
      }
      case "start": {
        const picked = o.base.filter((n) => o.ids.has(n.id));
        const most = Math.min(...picked.map((n) => n.length)) - (free ? MIN_BEATS : minLength);
        const d = clamp(snapTo(o.anchor.start + (beat - xBeat(view, o.x0)), free) - o.anchor.start, -Math.min(...picked.map((n) => n.start)), Math.max(0, most));
        return o.base.map((n) => (o.ids.has(n.id) ? { ...n, start: n.start + d, length: n.length - d } : n));
      }
      case "draw": {
        const end = free || step <= 0 ? beat : Math.ceil(beat / step - 1e-9) * step;
        const length = Math.max(free ? MIN_BEATS : minLength, end - o.note.start);
        o.note = { ...o.note, length };
        return [...o.base, o.note];
      }
      case "vel": {
        const v = velocityAt(y);
        const d = v - o.anchor.velocity;
        return o.base.map((n) => (n.id === o.anchor.id ? { ...n, velocity: v } : o.ids.has(n.id) ? { ...n, velocity: clamp(n.velocity + d, 1, 127) } : n));
      }
      default:
        return null;
    }
  };

  const shapeRegion = (o: Extract<Op, { kind: "regionStart" | "regionEnd" | "regionMove" }>, x: number, free: boolean) => {
    const beat = xBeat(view, x);
    const a = o.clip.offset;
    const b = o.clip.offset + o.clip.content;
    if (o.kind === "regionStart") return setRegion(o.clip, Math.max(0, Math.min(snapTo(beat, free), b - MIN_BEATS)), b);
    if (o.kind === "regionEnd") return setRegion(o.clip, a, Math.max(a + MIN_BEATS, snapTo(beat, free)));
    const d = Math.max(-a, snapTo(a + (beat - xBeat(view, o.x0)), free) - a);
    return setRegion(o.clip, a + d, b + d);
  };

  const onMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const { x, y } = local(e);
    const o = op.current;
    const el = e.currentTarget;
    if (!o) {
      const where = area(x, y);
      const hit = where === "grid" ? noteAt(x, y) : null;
      const ax = beatX(view, c.offset);
      const bx = beatX(view, c.offset + c.content);
      el.style.cursor =
        where === "keys"
          ? "pointer"
          : where === "ruler" && y <= BRACE_H + 2
            ? Math.abs(x - ax) <= NOTE_EDGE || Math.abs(x - bx) <= NOTE_EDGE
              ? "ew-resize"
              : x > ax && x < bx
                ? "grab"
                : "default"
            : where === "vel"
              ? stemAt(x, y)
                ? "ns-resize"
                : "default"
              : hit
                ? hit.zone === "move"
                  ? drawMode
                    ? "pointer"
                    : "grab"
                  : "ew-resize"
                : drawMode && where === "grid"
                  ? "crosshair"
                  : "default";
      return;
    }
    const free = ignoresGrid(e);
    if (o.kind === "key") {
      if (area(x, y) === "keys") {
        o.key = yKey(view, y);
        sound(o.key);
      }
      return;
    }
    if (o.kind === "marquee") {
      setMarquee({ x0: o.x0, y0: o.y0, x1: x, y1: y });
      const [l, r] = [Math.min(o.x0, x), Math.max(o.x0, x)];
      const [t, b] = [Math.min(o.y0, y), Math.max(o.y0, y)];
      const inside = notes.filter((n) => {
        const nx0 = beatX(view, n.start);
        const nx1 = beatX(view, n.start + n.length);
        const ny = keyY(view, n.key);
        return nx1 >= l && nx0 <= r && ny + view.rowH >= t && ny <= b;
      });
      setSelected(new Set([...o.keep, ...inside.map((n) => n.id)]));
      return;
    }
    if (o.kind === "regionStart" || o.kind === "regionEnd" || o.kind === "regionMove") {
      setShownClip(shapeRegion(o, x, free));
      return;
    }
    if ((o.kind === "move" || o.kind === "start" || o.kind === "end") && !o.moved) {
      if (Math.hypot(x - o.x0, y - o.y0) < DRAG_PX) return;
      o.moved = true;
    }
    if (o.kind === "draw") o.moved = true;
    const next = shape(o, x, y, free, e.altKey, true);
    if (next) setShown({ id: clip.id, notes: next });
  };

  const onUp = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const o = op.current;
    op.current = null;
    setMarquee(null);
    sound(null);
    if (!o) return;
    const { x, y } = local(e);
    const free = ignoresGrid(e);
    switch (o.kind) {
      case "move":
      case "start":
      case "end": {
        if (!o.moved) {
          // A click on a note with Draw on deletes it; otherwise it only selected it.
          if (o.erase) removeNotes(new Set([o.anchor.id]));
          else unshape();
          return;
        }
        const next = shape(o, x, y, free, e.altKey);
        if (!next) return;
        if (o.kind !== "move") lastLength.current = next.find((n) => n.id === o.anchor.id)?.length ?? lastLength.current;
        if (o.kind === "move" && e.altKey) setSelected(new Set(next.slice(o.base.length).map((n) => n.id)));
        commit(next, o.kind === "move" ? (e.altKey ? "copy" : "move") : "length");
        return;
      }
      case "draw": {
        const next = shape(o, x, y, free, false) ?? [...o.base, o.note];
        if (o.moved) lastLength.current = o.note.length;
        commit(next, "draw");
        return;
      }
      case "vel": {
        const next = shape(o, x, y, free, false);
        if (next) commit(next, "velocity");
        return;
      }
      case "regionStart":
      case "regionEnd":
      case "regionMove":
        commitClip(shapeRegion(o, x, free), "region");
        return;
      default:
        return;
    }
  };

  const onDouble = (e: React.MouseEvent<HTMLCanvasElement>) => {
    if (drawMode) return;
    const { x, y } = local(e);
    if (area(x, y) !== "grid") return;
    const hit = noteAt(x, y);
    if (hit) {
      removeNotes(new Set([hit.note.id]));
      return;
    }
    const note = addNote(xBeat(view, x), yKey(view, y), ignoresGrid(e));
    commit([...notes, note], "add");
    setSelected(new Set([note.id]));
  };

  // Wheel: up and down through the keys, sideways (or ⇧) through time, ⌘ zooms time, ⌥ the rows.
  useEffect(() => {
    const el = canvas.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      const x = e.clientX - r.left;
      const y = e.clientY - r.top;
      setView((v) => {
        if (e.ctrlKey || e.metaKey) {
          const at = xBeat(v, Math.max(KEY_W, x));
          const zoomX = clamp(v.zoomX * Math.exp(-e.deltaY * 0.01), MIN_ZOOM, MAX_ZOOM);
          return clampView({ ...v, zoomX, scrollX: at - (Math.max(KEY_W, x) - KEY_W) / zoomX });
        }
        if (e.altKey) {
          const key = (y - RULER_H + v.scrollY) / v.rowH;
          const rowH = clamp(v.rowH * Math.exp(-e.deltaY * 0.01), MIN_ROW, MAX_ROW);
          return clampView({ ...v, rowH, scrollY: key * rowH - (y - RULER_H) });
        }
        if (e.shiftKey || Math.abs(e.deltaX) > Math.abs(e.deltaY)) return clampView({ ...v, scrollX: v.scrollX + (e.deltaX || e.deltaY) / v.zoomX });
        return clampView({ ...v, scrollY: v.scrollY + e.deltaY });
      });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [clampView]);

  // The panel's height changes the rows in view: keep them in range.
  useEffect(() => setView((v) => clampView(v, size.h)), [size.h, clampView]);

  const onKey = (e: React.KeyboardEvent<HTMLElement>) => {
    if ((e.target as HTMLElement).tagName === "INPUT" || (e.target as HTMLElement).tagName === "SELECT") return;
    const k = e.key.toLowerCase();
    const cmd = e.metaKey || e.ctrlKey;
    const run = (f: () => void) => {
      e.preventDefault();
      e.stopPropagation();
      f();
    };
    if (cmd && !e.shiftKey && !e.altKey) {
      if (k === "a") return run(() => setSelected(new Set(notes.map((n) => n.id))));
      if (k === "d") return run(duplicate);
      return;
    }
    if (cmd || e.altKey) return;
    const by = step > 0 ? step : 0.25;
    if (e.key === "Backspace" || e.key === "Delete") return run(() => removeNotes(selected));
    if (e.key === "ArrowUp") return run(() => nudge(0, e.shiftKey ? 12 : 1));
    if (e.key === "ArrowDown") return run(() => nudge(0, e.shiftKey ? -12 : -1));
    if (e.key === "ArrowLeft") return run(() => nudge(e.shiftKey ? -bar : -by, 0));
    if (e.key === "ArrowRight") return run(() => nudge(e.shiftKey ? bar : by, 0));
    if (k === "q") return run(quantizeNotes);
    if (k === "b") return run(() => setDrawMode((d) => !d));
    if (e.key === "Escape") return run(() => (selected.size ? setSelected(new Set()) : onClose()));
  };

  const looped = isLoopedMidi(c);
  const count = selected.size;
  const commitName = () => {
    const n = name.trim();
    if (n && n !== clip.name) send({ ...toMidiInput(c, track.id), name: n }, `name:${clip.id}`, () => setName(clip.name));
    else setName(clip.name);
  };

  return (
    <section className="tl-notes" style={{ height: panel }} aria-label={`Notes of ${clip.name}`} tabIndex={-1} onKeyDown={onKey}>
      <div
        className="tl-notes-resize"
        title="Drag to change the note editor's height"
        onPointerDown={(e) => {
          e.preventDefault();
          capture(e);
          resize.current = { y0: e.clientY, h0: panel };
        }}
        onPointerMove={(e) => resize.current && setPanel(clamp(resize.current.h0 - (e.clientY - resize.current.y0), MIN_PANEL, MAX_PANEL))}
        onPointerUp={() => (resize.current = null)}
        onPointerCancel={() => (resize.current = null)}
      />
      <div className="tl-notes-bar">
        <span className="tl-notes-swatch" style={{ background: colour }} aria-hidden="true" />
        <input
          ref={nameBox}
          className="tl-notes-name"
          value={name}
          aria-label="Clip name"
          onChange={(e) => setName(e.target.value)}
          onBlur={commitName}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
            if (e.key === "Escape") {
              setName(clip.name);
              (e.currentTarget as HTMLInputElement).blur();
            }
          }}
        />
        <span className="tl-insp-pos mono" title="Where it starts and ends on the Timeline (bar.beat.sixteenth)">
          {position(c.start, signature)} – {position(midiEnd(c), signature)}
        </span>
        <span className="dim mono tl-notes-info" title="Notes, and the ones selected">
          {notes.length} {notes.length === 1 ? "note" : "notes"}
          {count ? ` · ${count} selected${count === 1 ? ` (${keyName(notes.find((n) => selected.has(n.id))?.key ?? 60)})` : ""}` : ""}
        </span>
        <span className="grow" />
        <button className={`tl-tool${drawMode ? " on" : ""}`} aria-pressed={drawMode} onClick={() => setDrawMode(!drawMode)} title="Draw: click to add notes, click a note to delete it (B)">
          <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
            <path d="M3 13l1-3.5 7-7 2.5 2.5-7 7zM9.5 4l2.5 2.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
          </svg>
          <span className="tl-tool-label">Draw</span>
        </button>
        <label className="len" title={`What notes snap to (hold ${mod} while dragging to ignore it)`}>
          <span>Grid</span>
          <select value={grid} onChange={(e) => setGrid(e.target.value as Grid)}>
            {GRIDS.map((g) => (
              <option key={g.id} value={g.id}>
                {g.label}
              </option>
            ))}
          </select>
        </label>
        <button className="btn small" onClick={quantizeNotes} disabled={!notes.length} title="Move the selected notes' starts (all of them when none is selected) onto the grid (Q)">
          Quantize
        </button>
        <label className="tl-insp-check" title="Repeat the loop on the ruler for as long as the clip plays">
          <input type="checkbox" checked={looped} onChange={(e) => commitClip(setLoopedMidi(c, e.target.checked), "loop")} />
          Loop
        </label>
        <button className="btn small" onClick={fit} title="Show the whole clip">
          Fit
        </button>
        <button className="btn small danger" onClick={onDelete} title="Delete the clip (⌫ on the Timeline)">
          Delete
        </button>
      </div>
      <div className="tl-notes-body" ref={box}>
        <canvas
          ref={canvas}
          className="tl-notes-canvas"
          style={{ width: size.w, height: size.h }}
          aria-label="Notes: a row for each key, time across"
          onPointerDown={onDown}
          onPointerMove={onMove}
          onPointerUp={onUp}
          onPointerCancel={() => {
            op.current = null;
            setMarquee(null);
            unshape();
            sound(null);
          }}
          onDoubleClick={onDouble}
        />
        <div className="tl-notes-head" ref={head} style={{ top: RULER_H, height: Math.max(0, gridBottom(size.h) - RULER_H) }} aria-hidden="true" />
      </div>
    </section>
  );
}
