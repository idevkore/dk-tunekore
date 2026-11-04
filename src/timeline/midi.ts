// MIDI clips' arithmetic (idevkore/dk-tunekore#38): pure functions, so the
// Timeline and the note editor can show an edit before the Rust side
// applies it (src-tauri/src/timeline/midi.rs clamps it again).
//
// Everything is in beats. A clip plays `length` beats of its notes from
// `offset + phase`, looping over `content` beats from `offset` when
// `phase + length` runs past them. Notes are in the clip's own time, from 0,
// so trimming or looping a clip moves no note.
import type { MidiClipIn, MidiClipView, MidiNote } from "./api";

/** The shortest note, clip or loop, in beats (the engine's MIN_BEATS). */
export const MIN_BEATS = 1 / 128;
/** A new note's velocity. */
export const DEFAULT_VELOCITY = 100;

export const midiEnd = (c: { start: number; length: number }) => c.start + c.length;

export const isLoopedMidi = (c: { phase: number; length: number; content: number }) => c.phase + c.length > c.content + 1e-9;

const mod = (a: number, n: number) => ((a % n) + n) % n;

export function toMidiInput(c: MidiClipView, track: number, withNotes = false): MidiClipIn {
  const base: MidiClipIn = { id: c.id, track, name: c.name, start: c.start, offset: c.offset, content: c.content, phase: c.phase, length: c.length };
  return withNotes ? { ...base, notes: c.notes } : base;
}

/** Moves the clip's start edge to `beat`, keeping its notes where they are in time. */
export function trimStartMidi(c: MidiClipView, beat: number): MidiClipView {
  const end = midiEnd(c);
  let start = Math.max(0, Math.min(beat, end - MIN_BEATS));
  let d = start - c.start;
  if (isLoopedMidi(c)) return { ...c, start, length: c.length - d, phase: mod(c.phase + d, c.content) };
  // Not before the clip's own time starts.
  if (c.offset + c.phase + d < 0) {
    d = -(c.offset + c.phase);
    start = c.start + d;
  }
  const at = c.offset + c.phase + d;
  return { ...c, start, offset: at, phase: 0, content: c.content - c.phase - d, length: c.length - d };
}

/** Moves the clip's end edge to `beat`. A clip that doesn't loop shows more or less of its notes. */
export function trimEndMidi(c: MidiClipView, beat: number): MidiClipView {
  const length = Math.max(MIN_BEATS, beat - c.start);
  if (isLoopedMidi(c)) return { ...c, length };
  return { ...c, length, content: c.phase + length };
}

/** Drags the clip's end out to `beat`, repeating what it plays now. */
export function loopEndMidi(c: MidiClipView, beat: number): MidiClipView {
  const length = Math.max(MIN_BEATS, beat - c.start);
  const content = isLoopedMidi(c) ? c.content : c.phase + c.length;
  return { ...c, length, content };
}

/** Turns looping on (two passes of what shows) or off (back to one pass). */
export function setLoopedMidi(c: MidiClipView, on: boolean): MidiClipView {
  if (on === isLoopedMidi(c)) return c;
  if (on) {
    const content = c.phase + c.length;
    return { ...c, content, length: c.length + (content - c.phase) };
  }
  return { ...c, length: Math.max(MIN_BEATS, c.content - c.phase) };
}

/** Sets the clip's region in its own time: the loop when it loops, the part
 *  it plays when it doesn't (and then its length follows). Where it starts
 *  playing stays put in its own time where it can. */
export function setRegion(c: MidiClipView, from: number, to: number): MidiClipView {
  const offset = Math.max(0, Math.min(from, to - MIN_BEATS));
  const content = Math.max(MIN_BEATS, to - offset);
  if (isLoopedMidi(c)) {
    const phase = Math.min(Math.max(0, c.offset + c.phase - offset), content - MIN_BEATS);
    return { ...c, offset, content, phase: mod(phase, content) };
  }
  return { ...c, offset, content, phase: 0, length: content };
}

/** Where in its own time the clip plays at `beat` on the Timeline, or null outside it. */
export function clipTimeAt(c: MidiClipView, beat: number): number | null {
  const t = beat - c.start;
  if (t < 0 || t >= c.length) return null;
  return c.offset + mod(c.phase + t, c.content);
}

/** Where each note plays on the Timeline, pass by pass, cut at the content's
 *  end and the clip's: [beat, length, note]. Notes outside the content don't play. */
export function* placed(c: MidiClipView, from = -Infinity, to = Infinity): Generator<[number, number, MidiNote]> {
  const end = midiEnd(c);
  const lo = Math.max(from, c.start);
  const hi = Math.min(to, end);
  if (lo >= hi || c.content <= 0) return;
  const base = c.start - c.phase;
  for (let k = Math.max(0, Math.floor((lo - base) / c.content) - 1); base + k * c.content < hi; k++) {
    const pass = base + k * c.content;
    for (const n of c.notes) {
      const at = n.start - c.offset;
      if (at < 0 || at >= c.content) continue;
      const beat = pass + at;
      if (beat < c.start || beat >= end) continue;
      const len = Math.min(n.length, c.content - at, end - beat);
      if (beat + len <= lo || beat >= hi) continue;
      yield [beat, len, n];
    }
    if (!isLoopedMidi(c)) break;
  }
}

/** Notes sorted as the Rust side keeps them. */
export const sortNotes = (notes: MidiNote[]) => [...notes].sort((a, b) => a.start - b.start || a.key - b.key || a.id - b.id);

/** An id no note of `notes` has. */
export const nextNoteId = (notes: MidiNote[]) => notes.reduce((m, n) => Math.max(m, n.id), 0) + 1;

/** The notes in `ids` (all of them when `ids` is empty) moved onto the grid:
 *  their starts, and with `ends` their ends too, never shorter than a step. */
export function quantize(notes: MidiNote[], ids: Set<number>, step: number, ends = false): MidiNote[] {
  if (step <= 0) return notes;
  const q = (x: number) => Math.round(x / step) * step;
  return notes.map((n) => {
    if (ids.size && !ids.has(n.id)) return n;
    const start = Math.max(0, q(n.start));
    const length = ends ? Math.max(step, q(n.start + n.length) - start) : n.length;
    return { ...n, start, length };
  });
}

const NAMES = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];

/** A key's name, with 60 as C3 (as the Sampler's pads count). */
export const keyName = (key: number) => `${NAMES[key % 12]}${Math.floor(key / 12) - 2}`;

export const isBlackKey = (key: number) => [1, 3, 6, 8, 10].includes(key % 12);
