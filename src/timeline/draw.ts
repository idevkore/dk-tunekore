// Drawing the Timeline's canvases: the lanes (grid, clips with their
// waveforms, fades and loops, the loop range, a drop ghost) and the ruler
// (bars and beats, the loop brace). The playhead is a DOM line over both,
// so it moves without a redraw.
import { COLOURS, type ClipView, type SourceView, type TrackView } from "./api";
import { barBeats, clipEnd, isLooped, secondsPerBeat, sourceAt } from "./edits";
import { getPeaks, levelFor } from "./peaks";

export const RULER_HEIGHT = 34;
/** The top strip of the ruler sets the loop; the rest seeks. */
export const LOOP_STRIP = 14;
/** The strip at the top of a clip with its name. */
export const CLIP_HEAD = 15;

/** A track as laid out: `top` from the top of the first track, unscrolled. */
export interface Lane {
  track: TrackView;
  top: number;
  height: number;
}

export interface Ghost {
  /** A lane index, or the lanes' length for a new track. */
  lane: number;
  start: number;
  /** In beats. */
  length: number;
  label: string;
}

export interface Scene {
  width: number;
  height: number;
  dpr: number;
  lanes: Lane[];
  /** Where the area for a new track starts (below the last lane). */
  end: number;
  scrollTop: number;
  /** The beat at x = 0. */
  scroll: number;
  /** Pixels per beat. */
  zoom: number;
  tempo: number;
  signature: [number, number];
  sources: Map<string, SourceView>;
  selected: Set<number>;
  loop: { on: boolean; start: number; end: number };
  ghost: Ghost | null;
  /** A clip under the pointer, to show its handles. */
  hover: number | null;
}

export const beatToX = (beat: number, scroll: number, zoom: number) => (beat - scroll) * zoom;
export const xToBeat = (x: number, scroll: number, zoom: number) => scroll + x / zoom;

/** Grid lines worth drawing at this zoom: a step in beats that is at least `minPx` apart. */
function gridSteps(zoom: number, bar: number): { bar: number; beat: number | null; fine: number | null } {
  let bars = 1;
  while (bars * bar * zoom < 14) bars *= 2;
  return { bar: bars * bar, beat: zoom >= 10 && bars === 1 ? 1 : null, fine: zoom >= 56 ? 0.25 : null };
}

function prepare(ctx: CanvasRenderingContext2D, w: number, h: number, dpr: number) {
  const c = ctx.canvas;
  if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
}

function verticals(ctx: CanvasRenderingContext2D, step: number, s: { scroll: number; zoom: number; width: number }, top: number, bottom: number, colour: string) {
  const first = Math.ceil(s.scroll / step) * step;
  ctx.fillStyle = colour;
  for (let b = first; ; b += step) {
    const x = Math.round(beatToX(b, s.scroll, s.zoom));
    if (x > s.width) break;
    ctx.fillRect(x, top, 1, bottom - top);
  }
}

function roundRect(ctx: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number) {
  ctx.beginPath();
  ctx.roundRect(x, y, Math.max(0, w), Math.max(0, h), Math.min(r, w / 2, h / 2));
}

/** A clip's gain at `t` seconds in, from its fades. */
function fadeGain(c: ClipView, t: number): number {
  let g = 1;
  if (c.fadeIn > 0 && t < c.fadeIn) g *= t / c.fadeIn;
  if (c.fadeOut > 0 && c.length - t < c.fadeOut) g *= (c.length - t) / c.fadeOut;
  return Math.max(0, g);
}

function drawWave(ctx: CanvasRenderingContext2D, c: ClipView, src: SourceView, x0: number, x1: number, y: number, h: number, s: Scene) {
  const spb = secondsPerBeat(s.tempo);
  const secondsPerPx = spb / s.zoom;
  const framesPerPx = secondsPerPx * src.rate;
  const peaks = getPeaks(c.path, src.version, levelFor(framesPerPx));
  if (!peaks) return;
  const { data, bucket } = peaks;
  const buckets = data.length / 2;
  const gain = Math.min(2, c.gainDb <= -60 ? 0 : Math.pow(10, c.gainDb / 20));
  const mid = y + h / 2;
  const half = (h / 2 - 1) / 127;
  const from = Math.max(Math.floor(x0), 0);
  const to = Math.min(Math.ceil(x1), s.width);
  for (let px = from; px < to; px++) {
    const t = (px - x0) * secondsPerPx;
    if (t < 0 || t >= c.length) continue;
    const f0 = sourceAt(c, t) * src.rate;
    const b0 = Math.max(0, Math.floor(f0 / bucket));
    const b1 = Math.min(buckets, Math.max(b0 + 1, Math.ceil((f0 + framesPerPx) / bucket)));
    let lo = 127;
    let hi = -127;
    for (let b = b0; b < b1; b++) {
      lo = Math.min(lo, data[2 * b]);
      hi = Math.max(hi, data[2 * b + 1]);
    }
    if (hi < lo) continue;
    const g = gain * fadeGain(c, t);
    const top = mid - hi * half * g;
    const bottom = mid - lo * half * g;
    ctx.fillRect(px, top, 1, Math.max(1, bottom - top));
  }
}

function drawClip(ctx: CanvasRenderingContext2D, c: ClipView, colour: string, lane: Lane, s: Scene) {
  const x0 = beatToX(c.start, s.scroll, s.zoom);
  const x1 = beatToX(clipEnd(c, s.tempo), s.scroll, s.zoom);
  if (x1 < 0 || x0 > s.width) return;
  const y = lane.top - s.scrollTop + 1;
  const h = lane.height - 2;
  const w = Math.max(2, x1 - x0);
  // The name strip; on short tracks, half the clip (as the Lanes' hit zones assume).
  const head = Math.min(CLIP_HEAD, h / 2);
  const src = s.sources.get(c.path);
  const missing = !src || src.missing;
  const selected = s.selected.has(c.id);
  ctx.save();
  roundRect(ctx, x0, y, w, h, 4);
  ctx.fillStyle = selected ? "#2c2546" : "#1d1b27";
  ctx.fill();
  ctx.clip();
  // The name strip.
  ctx.fillStyle = colour;
  ctx.globalAlpha = selected ? 0.95 : 0.7;
  ctx.fillRect(x0, y, w, head);
  ctx.globalAlpha = 1;
  const body = y + head;
  const bodyH = h - head;
  if (bodyH > 4 && !missing && src) {
    ctx.fillStyle = colour;
    ctx.globalAlpha = 0.85;
    drawWave(ctx, c, src, x0, x1, body + 1, bodyH - 2, s);
    ctx.globalAlpha = 1;
  }
  // Where a looped clip starts its content again.
  if (isLooped(c) && c.content > 0) {
    const spb = secondsPerBeat(s.tempo);
    ctx.fillStyle = "rgba(255,255,255,0.35)";
    for (let t = c.content - c.phase; t < c.length; t += c.content) {
      const x = Math.round(x0 + (t / spb) * s.zoom);
      if (x > s.width) break;
      ctx.fillRect(x, body, 1, bodyH);
      ctx.beginPath();
      ctx.moveTo(x - 3, body);
      ctx.lineTo(x + 3, body);
      ctx.lineTo(x, body + 4);
      ctx.fill();
    }
  }
  // Fades: a line from the corner, the faded part shaded.
  const spb = secondsPerBeat(s.tempo);
  const fin = (c.fadeIn / spb) * s.zoom;
  const fout = (c.fadeOut / spb) * s.zoom;
  ctx.strokeStyle = "rgba(255,255,255,0.7)";
  ctx.fillStyle = "rgba(12,12,16,0.45)";
  ctx.lineWidth = 1;
  if (fin > 1) {
    ctx.beginPath();
    ctx.moveTo(x0, y + h);
    ctx.lineTo(x0 + fin, body);
    ctx.lineTo(x0, body);
    ctx.closePath();
    ctx.fill();
    ctx.beginPath();
    ctx.moveTo(x0, y + h);
    ctx.lineTo(x0 + fin, body);
    ctx.stroke();
  }
  if (fout > 1) {
    ctx.beginPath();
    ctx.moveTo(x1, y + h);
    ctx.lineTo(x1 - fout, body);
    ctx.lineTo(x1, body);
    ctx.closePath();
    ctx.fill();
    ctx.beginPath();
    ctx.moveTo(x1, y + h);
    ctx.lineTo(x1 - fout, body);
    ctx.stroke();
  }
  // The name.
  if (w > 24 && h > 12) {
    ctx.fillStyle = "#0c0c10";
    ctx.font = "600 10.5px Geist, -apple-system, sans-serif";
    ctx.textBaseline = "middle";
    const label = missing ? `${c.name} (missing)` : c.name;
    ctx.fillText(label, Math.max(x0, 0) + 5, y + head / 2 + 0.5, Math.max(0, w - 10));
  }
  ctx.restore();
  // Outline, and the handles when it is under the pointer or selected.
  roundRect(ctx, x0 + 0.5, y + 0.5, w - 1, h - 1, 4);
  ctx.strokeStyle = selected ? "#f7f7f8" : missing ? "#ffb36b" : "rgba(255,255,255,0.12)";
  ctx.setLineDash(missing ? [4, 3] : []);
  ctx.stroke();
  ctx.setLineDash([]);
  if ((s.hover === c.id || selected) && h - head >= 9 && w >= 36) {
    ctx.fillStyle = "#f7f7f8";
    ctx.fillRect(Math.round(x0 + fin) - 3, body - 3, 6, 6);
    ctx.fillRect(Math.round(x1 - fout) - 3, body - 3, 6, 6);
    // The loop handle: the top of the right edge.
    ctx.fillStyle = "#b7ff00";
    ctx.fillRect(Math.round(x1) - 3, y + 1, 2, head - 2);
  }
}

export function drawLanes(ctx: CanvasRenderingContext2D, s: Scene) {
  prepare(ctx, s.width, s.height, s.dpr);
  const bar = barBeats(s.signature);
  // Lane backgrounds.
  for (const [i, lane] of s.lanes.entries()) {
    const y = lane.top - s.scrollTop;
    if (y > s.height || y + lane.height < 0) continue;
    ctx.fillStyle = i % 2 ? "#141419" : "#16161c";
    ctx.fillRect(0, y, s.width, lane.height);
    ctx.fillStyle = "#24242d";
    ctx.fillRect(0, y + lane.height - 1, s.width, 1);
  }
  const bottom = Math.min(s.height, s.end - s.scrollTop);
  // The loop range, shaded.
  if (s.loop.on) {
    const a = beatToX(s.loop.start, s.scroll, s.zoom);
    const b = beatToX(s.loop.end, s.scroll, s.zoom);
    ctx.fillStyle = "rgba(128,81,255,0.08)";
    ctx.fillRect(a, 0, b - a, bottom);
  }
  // The grid.
  const g = gridSteps(s.zoom, bar);
  if (g.fine) verticals(ctx, g.fine, s, 0, bottom, "rgba(255,255,255,0.025)");
  if (g.beat) verticals(ctx, g.beat, s, 0, bottom, "rgba(255,255,255,0.05)");
  verticals(ctx, g.bar, s, 0, bottom, "rgba(255,255,255,0.11)");
  // Clips.
  for (const lane of s.lanes) {
    const y = lane.top - s.scrollTop;
    if (y > s.height || y + lane.height < 0) continue;
    if (lane.track.kind !== "audio") {
      ctx.fillStyle = "#8a8a98";
      ctx.font = "11.5px Geist, -apple-system, sans-serif";
      ctx.textBaseline = "middle";
      const hint = lane.track.kind === "sampler" ? "The Sampler's pattern plays on this track, from bar 1." : "An instrument track: its notes come with MIDI clips.";
      ctx.fillText(hint, 12, y + lane.height / 2);
      continue;
    }
    const colour = COLOURS[lane.track.colour % COLOURS.length];
    for (const c of lane.track.clips) drawClip(ctx, c, colour, lane, s);
  }
  // A drop or drag on its way.
  if (s.ghost) {
    const lane = s.lanes[s.ghost.lane];
    const top = (lane ? lane.top : s.end) - s.scrollTop;
    const h = lane ? lane.height : 56;
    const x = beatToX(s.ghost.start, s.scroll, s.zoom);
    const w = Math.max(6, s.ghost.length * s.zoom);
    ctx.fillStyle = "rgba(183,255,0,0.14)";
    roundRect(ctx, x, top + 1, w, h - 2, 4);
    ctx.fill();
    ctx.strokeStyle = "#b7ff00";
    ctx.stroke();
    ctx.fillStyle = "#b7ff00";
    ctx.font = "600 10.5px Geist, -apple-system, sans-serif";
    ctx.textBaseline = "top";
    ctx.fillText(lane ? s.ghost.label : `${s.ghost.label} → new track`, x + 5, top + 4, Math.max(40, w - 10));
  }
}

export interface RulerScene {
  width: number;
  dpr: number;
  scroll: number;
  zoom: number;
  signature: [number, number];
  loop: { on: boolean; start: number; end: number };
}

export function drawRuler(ctx: CanvasRenderingContext2D, s: RulerScene) {
  const h = RULER_HEIGHT;
  prepare(ctx, s.width, h, s.dpr);
  ctx.fillStyle = "#0c0c10";
  ctx.fillRect(0, 0, s.width, h);
  ctx.fillStyle = "#121217";
  ctx.fillRect(0, 0, s.width, LOOP_STRIP);
  const bar = barBeats(s.signature);
  // The loop brace.
  const a = beatToX(s.loop.start, s.scroll, s.zoom);
  const b = beatToX(s.loop.end, s.scroll, s.zoom);
  ctx.fillStyle = s.loop.on ? "#8051ff" : "#3a3a46";
  roundRect(ctx, a, 2, b - a, LOOP_STRIP - 4, 3);
  ctx.fill();
  ctx.fillStyle = s.loop.on ? "#c9b8ff" : "#6a6a78";
  ctx.fillRect(a, 2, 2, LOOP_STRIP - 4);
  ctx.fillRect(b - 2, 2, 2, LOOP_STRIP - 4);
  // Ticks and bar numbers.
  const g = gridSteps(s.zoom, bar);
  let labelEvery = g.bar;
  while (labelEvery * s.zoom < 36) labelEvery *= 2;
  if (g.beat) verticals(ctx, g.beat, s, h - 5, h, "#3a3a46");
  verticals(ctx, g.bar, s, h - 10, h, "#5a5a68");
  ctx.fillStyle = "#a6a6b3";
  ctx.font = "10.5px 'Geist Mono', ui-monospace, Menlo, monospace";
  ctx.textBaseline = "middle";
  const first = Math.ceil(s.scroll / labelEvery) * labelEvery;
  for (let beat = first; ; beat += labelEvery) {
    const x = beatToX(beat, s.scroll, s.zoom);
    if (x > s.width) break;
    ctx.fillText(String(Math.round(beat / bar) + 1), x + 3, LOOP_STRIP + (h - LOOP_STRIP) / 2 - 2);
  }
  ctx.fillStyle = "#24242d";
  ctx.fillRect(0, h - 1, s.width, 1);
}
