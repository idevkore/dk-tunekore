// Drawing the note editor (NoteEditor.tsx) on one canvas: the ruler with the
// clip's loop brace along the top, the keyboard down the left, the notes on
// their rows, and the velocity lane along the bottom. Times are beats of the
// clip's own time; rows are MIDI keys, 127 at the top.
import type { MidiClipView, MidiNote } from "./api";
import { isBlackKey, isLoopedMidi, keyName } from "./midi";
import { alpha, mix, palette } from "../theme/theme";

export const KEY_W = 56;
export const RULER_H = 24;
/** The ruler's top strip, with the loop brace. */
export const BRACE_H = 9;
export const VEL_H = 64;
/** How close to an edge or a handle counts as on it, in pixels. */
export const NOTE_EDGE = 5;

/** Where the editor's view is: the beat at the grid's left edge, pixels per
 *  beat, the pixel offset of key 127's row from the grid's top, a row's height. */
export interface NoteView {
  scrollX: number;
  zoomX: number;
  scrollY: number;
  rowH: number;
}

export interface NoteScene {
  width: number;
  height: number;
  dpr: number;
  view: NoteView;
  clip: MidiClipView;
  notes: MidiNote[];
  selected: Set<number>;
  colour: string;
  barBeats: number;
  /** The grid step in beats; 0 for none. */
  step: number;
  /** A marquee being dragged, in canvas pixels. */
  marquee: { x0: number; y0: number; x1: number; y1: number } | null;
  /** Keys held on the keyboard. */
  held: Set<number>;
}

export const gridBottom = (height: number) => height - VEL_H;
export const beatX = (v: NoteView, beat: number) => KEY_W + (beat - v.scrollX) * v.zoomX;
export const xBeat = (v: NoteView, x: number) => v.scrollX + (x - KEY_W) / v.zoomX;
export const keyY = (v: NoteView, key: number) => RULER_H + (127 - key) * v.rowH - v.scrollY;
export const yKey = (v: NoteView, y: number) => Math.max(0, Math.min(127, 127 - Math.floor((y - RULER_H + v.scrollY) / v.rowH)));

function prepare(ctx: CanvasRenderingContext2D, w: number, h: number, dpr: number) {
  const c = ctx.canvas;
  if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
}

/** A grid line step worth drawing at this zoom: bars, beats and the grid's own step. */
function lineSteps(zoom: number, bar: number, step: number) {
  let bars = 1;
  while (bars * bar * zoom < 18) bars *= 2;
  return { bar: bars * bar, beat: zoom >= 14 && bars === 1 ? 1 : 0, fine: step > 0 && step < 1 && step * zoom >= 7 ? step : 0 };
}

function verticals(ctx: CanvasRenderingContext2D, v: NoteView, step: number, width: number, top: number, bottom: number, colour: string) {
  if (step <= 0) return;
  const first = Math.ceil(v.scrollX / step) * step;
  ctx.fillStyle = colour;
  for (let b = first; ; b += step) {
    const x = Math.round(beatX(v, b));
    if (x > width) break;
    if (x >= KEY_W) ctx.fillRect(x, top, 1, bottom - top);
  }
}

export function drawNotes(ctx: CanvasRenderingContext2D, s: NoteScene) {
  prepare(ctx, s.width, s.height, s.dpr);
  const p = palette();
  const v = s.view;
  const bottom = gridBottom(s.height);
  const dark = p.tone === "dark";
  const whiteRow = p.side;
  const blackRow = mix(p.side, p.deep, 0.6);
  const lines = lineSteps(v.zoomX, s.barBeats, s.step);
  const c = s.clip;
  const regionA = c.offset;
  const regionB = c.offset + c.content;

  // Rows, an octave line under each C.
  ctx.save();
  ctx.beginPath();
  ctx.rect(KEY_W, RULER_H, s.width - KEY_W, bottom - RULER_H);
  ctx.clip();
  const top = yKey(v, RULER_H);
  const low = yKey(v, bottom - 1);
  for (let k = top; k >= low; k--) {
    const y = keyY(v, k);
    ctx.fillStyle = isBlackKey(k) ? blackRow : whiteRow;
    ctx.fillRect(KEY_W, y, s.width - KEY_W, v.rowH);
    if (k % 12 === 0) {
      ctx.fillStyle = alpha(p.ink, 0.14);
      ctx.fillRect(KEY_W, y + v.rowH - 1, s.width - KEY_W, 1);
    }
  }
  verticals(ctx, v, lines.fine, s.width, RULER_H, bottom, alpha(p.ink, 0.03));
  verticals(ctx, v, lines.beat, s.width, RULER_H, bottom, alpha(p.ink, 0.07));
  verticals(ctx, v, lines.bar, s.width, RULER_H, bottom, alpha(p.ink, 0.15));
  // Outside the clip's content: shaded, as it doesn't play.
  ctx.fillStyle = alpha(p.deep, 0.55);
  const ax = beatX(v, regionA);
  const bx = beatX(v, regionB);
  if (ax > KEY_W) ctx.fillRect(KEY_W, RULER_H, ax - KEY_W, bottom - RULER_H);
  if (bx < s.width) ctx.fillRect(Math.max(KEY_W, bx), RULER_H, s.width - Math.max(KEY_W, bx), bottom - RULER_H);
  // Where the clip starts playing in its loop.
  if (isLoopedMidi(c) && c.phase > 0) {
    ctx.fillStyle = p.highlight;
    ctx.fillRect(Math.round(beatX(v, c.offset + c.phase)), RULER_H, 1, bottom - RULER_H);
  }
  // Notes.
  for (const n of s.notes) {
    const x0 = beatX(v, n.start);
    const x1 = beatX(v, n.start + n.length);
    if (x1 < KEY_W || x0 > s.width) continue;
    const y = keyY(v, n.key);
    if (y + v.rowH < RULER_H || y > bottom) continue;
    const plays = n.start >= regionA && n.start < regionB;
    const sel = s.selected.has(n.id);
    const w = Math.max(3, x1 - x0 - 1);
    ctx.globalAlpha = plays ? 1 : 0.4;
    ctx.fillStyle = mix(mix(p.deep, s.colour, 0.45 + 0.55 * (n.velocity / 127)), p.text, sel ? 0.35 : 0);
    ctx.beginPath();
    ctx.roundRect(x0 + 0.5, y + 0.5, w, Math.max(2, v.rowH - 1), Math.min(3, v.rowH / 3));
    ctx.fill();
    ctx.strokeStyle = sel ? p.text : alpha(p.ink, 0.35);
    ctx.lineWidth = 1;
    ctx.stroke();
    if (v.rowH >= 11 && w > 30) {
      ctx.fillStyle = alpha(dark ? p.ground : p.text, 0.85);
      ctx.font = "9.5px Geist, -apple-system, sans-serif";
      ctx.textBaseline = "middle";
      ctx.fillText(keyName(n.key), x0 + 4, y + v.rowH / 2 + 0.5, w - 6);
    }
    ctx.globalAlpha = 1;
  }
  if (s.marquee) {
    const m = s.marquee;
    const x = Math.min(m.x0, m.x1);
    const y = Math.min(m.y0, m.y1);
    ctx.fillStyle = alpha(p.accent, 0.12);
    ctx.fillRect(x, y, Math.abs(m.x1 - m.x0), Math.abs(m.y1 - m.y0));
    ctx.strokeStyle = p.accent;
    ctx.strokeRect(x + 0.5, y + 0.5, Math.abs(m.x1 - m.x0), Math.abs(m.y1 - m.y0));
  }
  ctx.restore();

  // The keyboard: a key for each row, the Cs named.
  ctx.save();
  ctx.beginPath();
  ctx.rect(0, RULER_H, KEY_W, bottom - RULER_H);
  ctx.clip();
  for (let k = top; k >= low; k--) {
    const y = keyY(v, k);
    const black = isBlackKey(k);
    const white = mix(p.side, p.text, dark ? 0.8 : 0.02);
    ctx.fillStyle = s.held.has(k) ? p.highlight : black ? (dark ? p.deep : mix(p.text, p.side, 0.3)) : white;
    ctx.fillRect(0, y, black ? KEY_W * 0.62 : KEY_W, v.rowH);
    if (black) {
      ctx.fillStyle = white;
      ctx.fillRect(KEY_W * 0.62, y, KEY_W * 0.38, v.rowH);
    }
    ctx.fillStyle = alpha(p.ink, 0.25);
    ctx.fillRect(0, y + v.rowH - 1, KEY_W, 1);
    if (k % 12 === 0 && v.rowH >= 8) {
      ctx.fillStyle = dark ? mix(p.deep, p.ground, 0.3) : p.dim;
      ctx.font = "600 9.5px 'Geist Mono', ui-monospace, Menlo, monospace";
      ctx.textBaseline = "middle";
      ctx.fillText(keyName(k), KEY_W - 28, y + v.rowH / 2 + 0.5);
    }
  }
  ctx.restore();
  ctx.fillStyle = p.line;
  ctx.fillRect(KEY_W - 1, RULER_H, 1, s.height - RULER_H);

  // The ruler: bars of the clip's own time, and the loop brace.
  ctx.fillStyle = p.deep;
  ctx.fillRect(KEY_W, 0, s.width - KEY_W, RULER_H);
  ctx.fillStyle = p.ground;
  ctx.fillRect(KEY_W, 0, s.width - KEY_W, BRACE_H + 2);
  ctx.save();
  ctx.beginPath();
  ctx.rect(KEY_W, 0, s.width - KEY_W, RULER_H);
  ctx.clip();
  const looped = isLoopedMidi(c);
  ctx.fillStyle = looped ? p.accent : mix(p.deep, p.text, 0.25);
  ctx.beginPath();
  ctx.roundRect(ax, 1, Math.max(2, bx - ax), BRACE_H, 3);
  ctx.fill();
  ctx.fillStyle = looped ? p.accentSoft : mix(p.deep, p.text, 0.45);
  ctx.fillRect(ax, 1, 3, BRACE_H);
  ctx.fillRect(bx - 3, 1, 3, BRACE_H);
  verticals(ctx, v, lines.beat, s.width, RULER_H - 5, RULER_H, mix(p.deep, p.text, 0.2));
  verticals(ctx, v, lines.bar, s.width, RULER_H - 9, RULER_H, mix(p.deep, p.text, 0.35));
  ctx.fillStyle = p.dim;
  ctx.font = "10px 'Geist Mono', ui-monospace, Menlo, monospace";
  ctx.textBaseline = "middle";
  let every = lines.bar;
  while (every * v.zoomX < 34) every *= 2;
  for (let b = Math.ceil(v.scrollX / every) * every; ; b += every) {
    const x = beatX(v, b);
    if (x > s.width) break;
    ctx.fillText(String(Math.round(b / s.barBeats) + 1), x + 3, BRACE_H + (RULER_H - BRACE_H) / 2 + 1);
  }
  ctx.restore();
  ctx.fillStyle = p.deep;
  ctx.fillRect(0, 0, KEY_W - 1, RULER_H);
  ctx.fillStyle = p.line;
  ctx.fillRect(0, RULER_H - 1, s.width, 1);

  // The velocity lane: a stem for each note.
  ctx.fillStyle = p.deep;
  ctx.fillRect(0, bottom, s.width, VEL_H);
  ctx.fillStyle = p.line;
  ctx.fillRect(0, bottom, s.width, 1);
  ctx.fillStyle = p.faint;
  ctx.font = "10px Geist, -apple-system, sans-serif";
  ctx.textBaseline = "top";
  ctx.fillText("Velocity", 6, bottom + 6);
  ctx.save();
  ctx.beginPath();
  ctx.rect(KEY_W, bottom + 1, s.width - KEY_W, VEL_H - 1);
  ctx.clip();
  verticals(ctx, v, lines.bar, s.width, bottom, s.height, alpha(p.ink, 0.1));
  const room = VEL_H - 10;
  for (const n of s.notes) {
    const x = Math.round(beatX(v, n.start)) + 0.5;
    if (x < KEY_W - 4 || x > s.width + 4) continue;
    const h = (n.velocity / 127) * room;
    const sel = s.selected.has(n.id);
    ctx.globalAlpha = n.start >= regionA && n.start < regionB ? 1 : 0.4;
    ctx.fillStyle = sel ? p.text : s.colour;
    ctx.fillRect(x - 0.5, s.height - 2 - h, 2, h);
    ctx.fillRect(x - 3, s.height - 2 - h - 2, 6, 4);
    ctx.globalAlpha = 1;
  }
  ctx.restore();
}
