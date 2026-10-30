// The Timeline's arithmetic: beats and seconds, the grid, and what each drag
// does to a clip. Pure functions, so the canvas can preview an edit before
// the Rust side applies it (src-tauri/src/timeline.rs clamps it again).
//
// A clip starts at `start` (beats) and plays its file from `offset + phase`
// for `length` seconds, looping over `content` seconds from `offset` when
// `phase + length` runs past it. Clips aren't warped yet, so a clip's length
// in beats follows the tempo.
import type { ClipIn, ClipView, Snap } from "./api";

/** The shortest clip, in seconds (the Rust side's MIN_LENGTH). */
export const MIN_LENGTH = 0.005;

export const secondsPerBeat = (tempo: number) => 60 / tempo;

/** Where a clip ends, in beats. */
export const clipEnd = (c: ClipView, tempo: number) => c.start + c.length / secondsPerBeat(tempo);

export const isLooped = (c: { phase: number; length: number; content: number }) => c.phase + c.length > c.content + 1e-9;

/** The grid's step in beats (0 = no grid). */
export function gridStep(snap: Snap, barBeats: number): number {
  switch (snap) {
    case "bar":
      return barBeats;
    case "beat":
      return 1;
    case "eighth":
      return 0.5;
    case "sixteenth":
      return 0.25;
    default:
      return 0;
  }
}

const isMac = typeof navigator !== "undefined" && navigator.platform.toLowerCase().includes("mac");

/** Whether a pointer event asks to ignore the grid: ⌘ held on the Mac, Ctrl elsewhere. */
export const ignoresGrid = (e: { metaKey: boolean; ctrlKey: boolean }) => (isMac ? e.metaKey : e.ctrlKey);

/** `beat` on the nearest grid line, or as it is with no grid or with `free` (the grid key held). */
export function snapBeat(beat: number, step: number, free = false): number {
  if (free || step <= 0) return beat;
  return Math.round(beat / step) * step;
}

/** A bar's length in beats (quarter notes): 4 in 4/4, 3 in 6/8. */
export const barBeats = (sig: [number, number]) => (sig[0] * 4) / sig[1];

/** "bar.beat.sixteenth", from 1, as DAWs count. */
export function position(beat: number, sig: [number, number]): string {
  const bb = barBeats(sig);
  const beatLen = 4 / sig[1];
  const b = Math.max(0, beat);
  const bar = Math.floor(b / bb + 1e-9);
  const inBar = b - bar * bb;
  const n = Math.floor(inBar / beatLen + 1e-9);
  const six = Math.floor((inBar - n * beatLen) / 0.25 + 1e-9);
  return `${bar + 1}.${n + 1}.${six + 1}`;
}

const mod = (a: number, n: number) => ((a % n) + n) % n;

export function toInput(c: ClipView, track: number): ClipIn {
  return {
    id: c.id,
    track,
    path: c.path,
    start: c.start,
    offset: c.offset,
    content: c.content,
    phase: c.phase,
    length: c.length,
    fadeIn: c.fadeIn,
    fadeOut: c.fadeOut,
    gainDb: c.gainDb,
  };
}

/** Keeps the fades inside the clip. */
function fitFades<T extends ClipView>(c: T): T {
  const fadeIn = Math.min(c.fadeIn, c.length);
  return { ...c, fadeIn, fadeOut: Math.min(c.fadeOut, c.length - fadeIn) };
}

/** Moves the clip's start edge to `beat`, keeping the audio where it is in time.
 *  A looped clip moves through its loop; one that isn't uncovers or hides the file's start. */
export function trimStart(c: ClipView, beat: number, tempo: number, seconds: number | null): ClipView {
  const spb = secondsPerBeat(tempo);
  const end = clipEnd(c, tempo);
  let start = Math.max(0, Math.min(beat, end - MIN_LENGTH / spb));
  let d = (start - c.start) * spb;
  if (isLooped(c)) {
    return fitFades({ ...c, start, length: c.length - d, phase: mod(c.phase + d, c.content) });
  }
  // Not past the file's first frame.
  if (c.offset + d < 0) {
    d = -c.offset;
    start = c.start + d / spb;
  }
  const content = c.content - d;
  if (seconds !== null && c.offset + d + content > seconds + 1e-9) return c;
  return fitFades({ ...c, start, offset: c.offset + d, content, length: c.length - d });
}

/** Moves the clip's end edge to `beat`. A clip that isn't looped stops at its file's end. */
export function trimEnd(c: ClipView, beat: number, tempo: number, seconds: number | null): ClipView {
  const spb = secondsPerBeat(tempo);
  let length = Math.max(MIN_LENGTH, (beat - c.start) * spb);
  if (isLooped(c)) return fitFades({ ...c, length });
  const room = seconds === null ? c.content - c.phase : seconds - c.offset - c.phase;
  length = Math.min(length, room);
  return fitFades({ ...c, length, content: c.phase + length });
}

/** Drags the clip's end out to `beat`, repeating what it plays now. */
export function loopEnd(c: ClipView, beat: number, tempo: number): ClipView {
  const spb = secondsPerBeat(tempo);
  const length = Math.max(MIN_LENGTH, (beat - c.start) * spb);
  // What shows now becomes the loop.
  const content = isLooped(c) ? c.content : c.phase + c.length;
  return fitFades({ ...c, length, content });
}

/** Turns looping on (two passes of what shows) or off (back to one pass). */
export function setLooped(c: ClipView, on: boolean): ClipView {
  if (on === isLooped(c)) return c;
  if (on) {
    const content = c.phase + c.length;
    return fitFades({ ...c, content, length: c.length + (content - c.phase) });
  }
  return fitFades({ ...c, length: Math.max(MIN_LENGTH, c.content - c.phase) });
}

/** Sets a fade from a pointer at `beat` inside the clip. */
export function setFade(c: ClipView, which: "in" | "out", beat: number, tempo: number): ClipView {
  const spb = secondsPerBeat(tempo);
  if (which === "in") {
    const fadeIn = Math.max(0, Math.min((beat - c.start) * spb, c.length - c.fadeOut));
    return { ...c, fadeIn };
  }
  const fadeOut = Math.max(0, Math.min((clipEnd(c, tempo) - beat) * spb, c.length - c.fadeIn));
  return { ...c, fadeOut };
}

/** The seconds into the clip's file that play at `t` seconds into the clip. */
export const sourceAt = (c: ClipView, t: number) => c.offset + mod(c.phase + t, c.content);
