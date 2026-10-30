// The bar and beat ruler over the lanes. Its top strip holds the loop
// brace: drag across the strip for a new loop range, drag the brace to move
// it or its ends to resize it, click it to turn the loop on or off. Below
// the strip, a click or a drag moves the playhead. Both snap to the grid
// unless ⌘ is held.
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { beatToX, drawRuler, LOOP_STRIP, RULER_HEIGHT, xToBeat } from "./draw";
import { snapBeat } from "./edits";

/** Keeps the pointer's events coming here through a drag (a pointer that can't be captured just isn't). */
function capture(e: React.PointerEvent<Element>) {
  try {
    e.currentTarget.setPointerCapture(e.pointerId);
  } catch {
    // a synthetic or already-released pointer
  }
}

type Loop = { on: boolean; start: number; end: number };

type Props = {
  width: number;
  scroll: number;
  zoom: number;
  signature: [number, number];
  loop: Loop;
  step: number;
  onLoop: (loop: Loop) => void;
  onSeek: (beat: number) => void;
  onZoom: (factor: number, atX: number) => void;
  onPan: (beats: number) => void;
};

type Drag = { kind: "new" | "move" | "start" | "end"; x0: number; loop0: Loop; moved: boolean } | { kind: "seek" };

export default function Ruler({ width, scroll, zoom, signature, loop, step, onLoop, onSeek, onZoom, onPan }: Props) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const drag = useRef<Drag | null>(null);
  const [shown, setShown] = useState<Loop | null>(null);
  const latest = useRef({ zoom, onZoom, onPan });
  latest.current = { zoom, onZoom, onPan };

  useEffect(() => setShown(null), [loop]);

  useLayoutEffect(() => {
    const ctx = canvas.current?.getContext("2d");
    if (!ctx || width <= 0) return;
    drawRuler(ctx, { width, dpr: window.devicePixelRatio || 1, scroll, zoom, signature, loop: shown ?? loop });
  }, [width, scroll, zoom, signature, loop, shown]);

  useEffect(() => {
    const c = canvas.current;
    if (!c) return;
    const onWheel = (e: WheelEvent) => {
      const p = latest.current;
      e.preventDefault();
      if (e.ctrlKey || e.metaKey) p.onZoom(Math.exp(-e.deltaY * 0.01), e.clientX - c.getBoundingClientRect().left);
      else p.onPan((e.deltaX || e.deltaY) / p.zoom);
    };
    c.addEventListener("wheel", onWheel, { passive: false });
    return () => c.removeEventListener("wheel", onWheel);
  }, []);

  const xOf = (e: React.PointerEvent) => e.clientX - canvas.current!.getBoundingClientRect().left;
  const yOf = (e: React.PointerEvent) => e.clientY - canvas.current!.getBoundingClientRect().top;
  const at = (x: number, free: boolean) => Math.max(0, snapBeat(xToBeat(x, scroll, zoom), step, free));
  const minLength = step > 0 ? step : 0.25;

  const loopFor = (d: Exclude<Drag, { kind: "seek" }>, x: number, free: boolean): Loop => {
    const b = at(x, free);
    const l = d.loop0;
    switch (d.kind) {
      case "new": {
        const a = at(d.x0, free);
        return { on: true, start: Math.min(a, b), end: Math.max(Math.max(a, b), Math.min(a, b) + minLength) };
      }
      case "move": {
        const delta = Math.max(-l.start, snapBeat(xToBeat(x, scroll, zoom) - xToBeat(d.x0, scroll, zoom), step, free));
        return { ...l, start: l.start + delta, end: l.end + delta };
      }
      case "start":
        return { ...l, start: Math.min(b, l.end - minLength) };
      case "end":
        return { ...l, end: Math.max(b, l.start + minLength) };
    }
  };

  const onDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (e.button !== 0) return;
    capture(e);
    const x = xOf(e);
    if (yOf(e) > LOOP_STRIP) {
      drag.current = { kind: "seek" };
      onSeek(at(x, e.metaKey));
      return;
    }
    const a = beatToX(loop.start, scroll, zoom);
    const b = beatToX(loop.end, scroll, zoom);
    const kind = Math.abs(x - a) <= 6 ? "start" : Math.abs(x - b) <= 6 ? "end" : x > a && x < b ? "move" : "new";
    drag.current = { kind, x0: x, loop0: loop, moved: false };
  };

  const onMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const d = drag.current;
    const x = xOf(e);
    if (!d) {
      const a = beatToX(loop.start, scroll, zoom);
      const b = beatToX(loop.end, scroll, zoom);
      e.currentTarget.style.cursor = yOf(e) > LOOP_STRIP ? "text" : Math.abs(x - a) <= 6 || Math.abs(x - b) <= 6 ? "ew-resize" : x > a && x < b ? "grab" : "copy";
      return;
    }
    if (d.kind === "seek") {
      onSeek(at(x, e.metaKey));
      return;
    }
    if (!d.moved && Math.abs(x - d.x0) < 3) return;
    d.moved = true;
    setShown(loopFor(d, x, e.metaKey));
  };

  const onUp = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const d = drag.current;
    drag.current = null;
    if (!d || d.kind === "seek") return;
    if (!d.moved) {
      // A click on the brace turns the loop on or off; elsewhere in the strip, nothing.
      if (d.kind === "move") onLoop({ ...loop, on: !loop.on });
      setShown(null);
      return;
    }
    const next = loopFor(d, xOf(e), e.metaKey);
    setShown(next);
    onLoop(next);
  };

  return (
    <canvas
      ref={canvas}
      className="tl-ruler"
      style={{ width, height: RULER_HEIGHT }}
      aria-label="Ruler: click to move the playhead; drag in the top strip to set the loop"
      onPointerDown={onDown}
      onPointerMove={onMove}
      onPointerUp={onUp}
      onPointerCancel={() => {
        drag.current = null;
        setShown(null);
      }}
    />
  );
}
