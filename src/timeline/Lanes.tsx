// The tracks' lanes, on one canvas: clips to select, move (⌥ copies),
// trim at either edge, loop by the top of the right edge, and (audio clips)
// fade by the handles at their top corners; a click on empty space moves
// the playhead, and a double-click on an instrument track's lane adds an
// empty MIDI clip there. Audio clips move between audio tracks, MIDI clips
// between instrument tracks. Drags snap to the grid unless ⌘ (Ctrl on
// Windows and Linux) is held. While a drag runs, the canvas shows the edit
// as it will be; on release it goes to the Rust side as one undoable edit,
// and the preview stays until the new state arrives.
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ClipView, Edit, MidiClipView, SourceView } from "./api";
import { useTheme } from "../theme/theme";
import { beatToX, CLIP_HEAD, drawLanes, xToBeat, type Ghost, type Lane } from "./draw";
import { clipEnd, ignoresGrid, loopEnd, secondsPerBeat, setFade, snapBeat, toInput, trimEnd, trimStart } from "./edits";
import { loopEndMidi, midiEnd, toMidiInput, trimEndMidi, trimStartMidi } from "./midi";
import { onPeaks } from "./peaks";

/** Keeps the pointer's events coming here through a drag (a pointer that can't be captured just isn't). */
function capture(e: React.PointerEvent<Element>) {
  try {
    e.currentTarget.setPointerCapture(e.pointerId);
  } catch {
    // a synthetic or already-released pointer
  }
}

type Zone = "move" | "start" | "end" | "loop" | "fadeIn" | "fadeOut";
type Shaping = "start" | "end" | "loop" | "fadeIn" | "fadeOut";

/** A clip of either kind, as a drag holds it. */
type Held = { midi: false; clip: ClipView } | { midi: true; clip: MidiClipView };
type Picked = Held & { lane: number };

type Op =
  | { kind: "move"; x0: number; y0: number; anchor: Held; picked: Picked[]; moved: boolean }
  | { kind: Shaping; x0: number; y0: number; held: Held; lane: number; moved: boolean };

/** Where a clip of either kind starts and ends on the Timeline, in beats. */
const span = (h: Held, tempo: number) => (h.midi ? [h.clip.start, midiEnd(h.clip)] : [h.clip.start, clipEnd(h.clip, tempo)]);

const CURSORS: Record<Zone, string> = { move: "grab", start: "ew-resize", end: "ew-resize", loop: "e-resize", fadeIn: "ew-resize", fadeOut: "ew-resize" };
/** How close to an edge or handle counts as on it, in pixels. */
const EDGE = 6;
/** Movement before a press becomes a drag. */
const DRAG_PX = 3;

type Props = {
  lanes: Lane[];
  /** Where the area for a new track starts. */
  end: number;
  width: number;
  height: number;
  scroll: number;
  zoom: number;
  scrollTop: number;
  tempo: number;
  signature: [number, number];
  sources: Map<string, SourceView>;
  selected: Set<number>;
  loop: { on: boolean; start: number; end: number };
  /** The grid step in beats; 0 for none. */
  step: number;
  /** A Library or Finder drop on its way. */
  ghost: Ghost | null;
  onSelect: (ids: number[]) => void;
  onTrack: (id: number) => void;
  /** A double-click on an instrument track's lane, at a beat on the grid. */
  onCreate: (track: number, beat: number) => void;
  onSeek: (beat: number) => void;
  /** Sends the edit; a refusal drops the preview. */
  onEdit: (edit: Edit) => Promise<unknown>;
  onZoom: (factor: number, atX: number) => void;
  onPan: (beats: number) => void;
};

export default function Lanes(props: Props) {
  const { lanes, width, height, scroll, zoom, scrollTop, tempo, step } = props;
  const canvas = useRef<HTMLCanvasElement>(null);
  const op = useRef<Op | null>(null);
  const [preview, setPreview] = useState<{ lanes: Lane[]; ghost: Ghost | null; edit: Edit | null } | null>(null);
  const [hover, setHover] = useState<number | null>(null);
  const [redraw, setRedraw] = useState(0);
  const latest = useRef(props);
  latest.current = props;

  // New state from the Rust side: the edit landed, the preview goes.
  useEffect(() => {
    if (!op.current) setPreview(null);
  }, [lanes]);

  useEffect(() => onPeaks(() => setRedraw((n) => n + 1)), []);
  // A new colour theme: draw again with its colours.
  const theme = useTheme();

  useLayoutEffect(() => {
    const ctx = canvas.current?.getContext("2d");
    if (!ctx || width <= 0 || height <= 0) return;
    drawLanes(ctx, {
      width,
      height,
      dpr: window.devicePixelRatio || 1,
      lanes: preview?.lanes ?? lanes,
      end: props.end,
      scrollTop,
      scroll,
      zoom,
      tempo,
      signature: props.signature,
      sources: props.sources,
      selected: props.selected,
      loop: props.loop,
      ghost: preview?.ghost ?? props.ghost,
      hover,
    });
  }, [lanes, preview, width, height, scroll, zoom, scrollTop, tempo, props.end, props.signature, props.sources, props.selected, props.loop, props.ghost, hover, redraw, theme]);

  // Wheel: ⌘ or Ctrl (and pinch) zooms around the pointer, sideways (or ⇧) scrolls in time.
  useEffect(() => {
    const c = canvas.current;
    if (!c) return;
    const onWheel = (e: WheelEvent) => {
      const p = latest.current;
      if (e.ctrlKey || e.metaKey) {
        e.preventDefault();
        p.onZoom(Math.exp(-e.deltaY * 0.01), e.clientX - c.getBoundingClientRect().left);
      } else if (e.shiftKey || Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
        e.preventDefault();
        const d = e.deltaX || e.deltaY;
        p.onPan(d / p.zoom);
      }
    };
    c.addEventListener("wheel", onWheel, { passive: false });
    return () => c.removeEventListener("wheel", onWheel);
  }, []);

  const local = (e: { clientX: number; clientY: number }) => {
    const r = canvas.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  /** The lane at `y` (canvas pixels), or `lanes.length` below the last one. */
  const laneAt = (y: number) => {
    const at = y + scrollTop;
    const i = lanes.findIndex((l) => at >= l.top && at < l.top + l.height);
    return i >= 0 ? i : at >= props.end ? lanes.length : -1;
  };

  const hit = (x: number, y: number): { lane: number; held?: Held; zone?: Zone } => {
    const lane = laneAt(y);
    const l = lanes[lane];
    if (l?.track.kind === "instrument") {
      for (let i = l.track.midi.length - 1; i >= 0; i--) {
        const c = l.track.midi[i];
        const x0 = beatToX(c.start, scroll, zoom);
        const x1 = beatToX(midiEnd(c), scroll, zoom);
        if (x < x0 - 2 || x > x1 + 2) continue;
        const top = l.top - scrollTop + 1;
        const head = Math.min(CLIP_HEAD, (l.height - 2) / 2);
        const edge = Math.min(EDGE, (x1 - x0) / 3);
        let zone: Zone = "move";
        if (x - x0 <= edge) zone = "start";
        else if (x1 - x <= edge) zone = y < top + head ? "loop" : "end";
        return { lane, held: { midi: true, clip: c }, zone };
      }
      return { lane };
    }
    if (!l || l.track.kind !== "audio") return { lane };
    const spb = secondsPerBeat(tempo);
    // The topmost (last drawn) first.
    for (let i = l.track.clips.length - 1; i >= 0; i--) {
      const c = l.track.clips[i];
      const x0 = beatToX(c.start, scroll, zoom);
      const x1 = beatToX(clipEnd(c, tempo), scroll, zoom);
      if (x < x0 - 2 || x > x1 + 2) continue;
      const top = l.top - scrollTop + 1;
      const h = l.height - 2;
      const head = Math.min(CLIP_HEAD, h / 2);
      const body = top + head;
      const w = x1 - x0;
      // Narrow clips and short tracks keep their middle third for moving.
      const edge = Math.min(EDGE, w / 3);
      const tall = Math.min(EDGE, (h - head) / 3);
      const fin = x0 + (c.fadeIn / spb) * zoom;
      const fout = x1 - (c.fadeOut / spb) * zoom;
      let zone: Zone = "move";
      if (w >= 6 * EDGE && tall >= 3 && Math.abs(y - body) <= tall && Math.abs(x - fin) <= edge) zone = "fadeIn";
      else if (w >= 6 * EDGE && tall >= 3 && Math.abs(y - body) <= tall && Math.abs(x - fout) <= edge) zone = "fadeOut";
      else if (x - x0 <= edge) zone = "start";
      else if (x1 - x <= edge) zone = y < body ? "loop" : "end";
      return { lane, held: { midi: false, clip: c }, zone };
    }
    return { lane };
  };

  /** What the drag does with the pointer at (x, y): the lanes to show and the edit to send. */
  const shape = (o: Op, x: number, y: number, free: boolean, copy: boolean) => {
    const beat = xToBeat(x, scroll, zoom);
    if (o.kind === "move") {
      const dx = beat - xToBeat(o.x0, scroll, zoom);
      const first = Math.min(...o.picked.map((p) => p.clip.start));
      const d = Math.max(snapBeat(o.anchor.clip.start + dx, step, free) - o.anchor.clip.start, -first);
      const at = laneAt(y);
      const from = lanes.findIndex((l) => (o.anchor.midi ? l.track.midi : l.track.clips).some((c) => c.id === o.anchor.clip.id));
      // Below the last track only audio clips make a track of their own (an instrument track needs its plugin).
      const anyMidi = o.picked.some((p) => p.midi);
      const toNew = at >= lanes.length && !anyMidi;
      let dl = 0;
      if (!toNew && at >= 0) {
        const lo = Math.min(...o.picked.map((p) => p.lane));
        const hi = Math.max(...o.picked.map((p) => p.lane));
        const want = Math.max(-lo, Math.min(lanes.length - 1 - hi, Math.min(at, lanes.length - 1) - from));
        // Each clip stays on a track of its kind (audio clips on audio tracks, MIDI clips on
        // instrument tracks): the nearest move to the pointer where they all do.
        const fits = (k: number) => o.picked.every((p) => lanes[p.lane + k]?.track.kind === (p.midi ? "instrument" : "audio"));
        for (let k = want; k !== 0; k -= Math.sign(want)) {
          if (fits(k)) {
            dl = k;
            break;
          }
        }
      }
      const ids = new Set(o.picked.map((p) => p.clip.id));
      const moved = o.picked.map((p) => ({ ...p, clip: { ...p.clip, start: p.clip.start + d }, lane: toNew ? lanes.length : p.lane + dl }) as Picked);
      const shown = lanes.map((l, i) => {
        const here = moved.filter((m) => m.lane === i);
        const copyOf = <T extends { id: number }>(c: T): T => (copy ? { ...c, id: -c.id } : c);
        return {
          ...l,
          track: {
            ...l.track,
            clips: [...l.track.clips.filter((c) => copy || !ids.has(c.id)), ...here.flatMap((m) => (m.midi ? [] : [copyOf(m.clip)]))],
            midi: [...l.track.midi.filter((c) => copy || !ids.has(c.id)), ...here.flatMap((m) => (m.midi ? [copyOf(m.clip)] : []))],
          },
        };
      });
      const [a0, a1] = span(o.anchor, tempo);
      const ghost: Ghost | null = toNew ? { lane: lanes.length, start: a0 + d, length: a1 - a0, label: o.anchor.clip.name } : null;
      const edit: Edit = {
        key: copy ? "copy" : "move",
        clips: moved.flatMap((m) => (m.midi ? [] : [{ ...toInput(m.clip, toNew ? 0 : lanes[m.lane].track.id), id: copy ? null : m.clip.id }])),
        // A copy takes the notes with it; a move leaves them where they are, in the clip.
        midi: moved.flatMap((m) => (m.midi ? [{ ...toMidiInput(m.clip, lanes[m.lane].track.id, copy), id: copy ? null : m.clip.id }] : [])),
        delete: [],
      };
      return { lanes: shown, ghost, edit };
    }
    const snapped = snapBeat(beat, step, free);
    if (o.held.midi) {
      const h = o.held.clip;
      const c = o.kind === "start" ? trimStartMidi(h, snapped) : o.kind === "end" ? trimEndMidi(h, snapped) : o.kind === "loop" ? loopEndMidi(h, snapped) : h;
      const shown = lanes.map((l, i) => (i === o.lane ? { ...l, track: { ...l.track, midi: l.track.midi.map((x) => (x.id === c.id ? c : x)) } } : l));
      const edit: Edit = { key: `${o.kind}:${c.id}`, clips: [], midi: [toMidiInput(c, lanes[o.lane].track.id)], delete: [] };
      return { lanes: shown, ghost: null, edit };
    }
    const h = o.held.clip;
    const src = props.sources.get(h.path);
    const seconds = src && !src.missing ? src.seconds : null;
    const c =
      o.kind === "start"
        ? trimStart(h, snapped, tempo, seconds)
        : o.kind === "end"
          ? trimEnd(h, snapped, tempo, seconds)
          : o.kind === "loop"
            ? loopEnd(h, snapped, tempo)
            : setFade(h, o.kind === "fadeIn" ? "in" : "out", beat, tempo);
    const shown = lanes.map((l, i) => (i === o.lane ? { ...l, track: { ...l.track, clips: l.track.clips.map((x) => (x.id === c.id ? c : x)) } } : l));
    const edit: Edit = { key: `${o.kind}:${c.id}`, clips: [toInput(c, lanes[o.lane].track.id)], delete: [] };
    return { lanes: shown, ghost: null, edit };
  };

  const onDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (e.button !== 0) return;
    const { x, y } = local(e);
    const h = hit(x, y);
    const lane = lanes[h.lane];
    if (lane) props.onTrack(lane.track.id);
    if (!h.held) {
      props.onSelect([]);
      if (h.lane >= 0) props.onSeek(Math.max(0, snapBeat(xToBeat(x, scroll, zoom), step, ignoresGrid(e))));
      return;
    }
    capture(e);
    const c = h.held.clip;
    // Shift adds to the selection or takes the clip out of it.
    let sel: Set<number>;
    if (e.shiftKey) {
      sel = new Set(props.selected);
      if (sel.has(c.id)) sel.delete(c.id);
      else sel.add(c.id);
    } else sel = props.selected.has(c.id) ? props.selected : new Set([c.id]);
    props.onSelect([...sel]);
    if (h.zone === "move" || !h.zone) {
      if (!sel.has(c.id)) return;
      const picked: Picked[] = lanes.flatMap((l, i) => [
        ...l.track.clips.filter((x) => sel.has(x.id)).map((clip) => ({ midi: false as const, clip, lane: i })),
        ...l.track.midi.filter((x) => sel.has(x.id)).map((clip) => ({ midi: true as const, clip, lane: i })),
      ]);
      op.current = { kind: "move", x0: x, y0: y, anchor: h.held, picked, moved: false };
      e.currentTarget.style.cursor = "grabbing";
    } else {
      op.current = { kind: h.zone, x0: x, y0: y, held: h.held, lane: h.lane, moved: false };
    }
  };

  // A double-click on an instrument track's empty lane: a new MIDI clip, from the grid line before it.
  const onDouble = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const { x, y } = local(e);
    const h = hit(x, y);
    const lane = lanes[h.lane];
    if (h.held || lane?.track.kind !== "instrument") return;
    const beat = xToBeat(x, scroll, zoom);
    const at = step > 0 && !ignoresGrid(e) ? Math.floor(beat / step) * step : beat;
    props.onCreate(lane.track.id, Math.max(0, at));
  };

  const onMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const { x, y } = local(e);
    const o = op.current;
    if (!o) {
      const h = hit(x, y);
      setHover(h.held?.clip.id ?? null);
      e.currentTarget.style.cursor = h.zone ? CURSORS[h.zone] : "default";
      return;
    }
    if (!o.moved && Math.hypot(x - o.x0, y - o.y0) < DRAG_PX) return;
    o.moved = true;
    setPreview(shape(o, x, y, ignoresGrid(e), e.altKey));
  };

  const onUp = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const o = op.current;
    op.current = null;
    e.currentTarget.style.cursor = "default";
    if (!o || !o.moved) {
      setPreview(null);
      return;
    }
    const { x, y } = local(e);
    const p = shape(o, x, y, ignoresGrid(e), e.altKey);
    setPreview(p);
    if (p.edit) props.onEdit(p.edit).catch(() => setPreview(null));
  };

  return (
    <canvas
      ref={canvas}
      className="tl-lanes"
      style={{ width, height }}
      aria-label="Tracks and clips"
      onPointerDown={onDown}
      onPointerMove={onMove}
      onPointerUp={onUp}
      onDoubleClick={onDouble}
      onPointerCancel={() => {
        op.current = null;
        setPreview(null);
      }}
      onPointerLeave={() => !op.current && setHover(null)}
    />
  );
}
