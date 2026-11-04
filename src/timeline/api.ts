// Typed wrappers for the Timeline's commands and events (src-tauri/src/timeline.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Snap = "bar" | "beat" | "eighth" | "sixteenth" | "off";

export interface TrackMix {
  /** Linear, 0..4. */
  volume: number;
  /** -1..1. */
  pan: number;
  mute: boolean;
  solo: boolean;
}

/** A clip: `start` in beats, the rest in seconds of its file. */
export interface ClipView {
  id: number;
  path: string;
  name: string;
  start: number;
  offset: number;
  content: number;
  phase: number;
  length: number;
  fadeIn: number;
  fadeOut: number;
  gainDb: number;
}

/** A note, in beats of its clip's own time. `id` names it within its clip. */
export interface MidiNote {
  id: number;
  /** 0..127; 60 is C3. */
  key: number;
  start: number;
  length: number;
  /** 1..127. */
  velocity: number;
}

/** A MIDI clip: all in beats. It plays `length` beats of its notes from
 *  `offset + phase`, looping over `content` beats from `offset` when
 *  `phase + length` runs past them, as an audio clip loops over its file. */
export interface MidiClipView {
  id: number;
  name: string;
  start: number;
  offset: number;
  content: number;
  phase: number;
  length: number;
  /** Sorted by start, then key. */
  notes: MidiNote[];
}

export interface TrackView {
  id: number;
  name: string;
  /** Audio clips go on audio tracks, MIDI clips on instrument tracks; the Sampler's shows its mix. */
  kind: "sampler" | "audio" | "instrument";
  /** An index into COLOURS. */
  colour: number;
  height: number;
  mix: TrackMix;
  clips: ClipView[];
  midi: MidiClipView[];
}

export interface SourceView {
  path: string;
  seconds: number;
  channels: number;
  frames: number;
  rate: number;
  missing: boolean;
  /** Changes when the file is read again after it changed on disk. */
  version: number;
}

export interface TimelineView {
  tracks: TrackView[];
  sources: SourceView[];
  tempo: number;
  signature: [number, number];
  zoom: number;
  scroll: number;
  snap: Snap;
  library: boolean;
  loopOn: boolean;
  loopStart: number;
  loopEnd: number;
  canUndo: boolean;
  canRedo: boolean;
}

export interface Tick {
  playing: boolean;
  /** Where it plays now, or where play starts while stopped. */
  beat: number;
  tempo: number;
  signature: [number, number];
  /** `[track id, peak]` since the last tick. */
  levels: [number, number][];
}

/** A clip to add (no id) or change. `track` 0 puts it on a new track. */
export interface ClipIn {
  id?: number | null;
  track: number;
  path: string;
  start: number;
  offset: number;
  content: number;
  phase: number;
  length: number;
  fadeIn: number;
  fadeOut: number;
  gainDb: number;
}

/** A MIDI clip to add (no id) or change, on an instrument track. Without
 *  `notes`, an existing clip keeps its own and a new one starts empty. */
export interface MidiClipIn {
  id?: number | null;
  track: number;
  name?: string;
  start: number;
  offset: number;
  content: number;
  phase: number;
  length: number;
  notes?: MidiNote[];
}

export interface Edit {
  /** The kind of edit: a run of the same kind undoes as one step. */
  key: string;
  clips: ClipIn[];
  midi?: MidiClipIn[];
  /** Clips of either kind. */
  delete: number[];
}

export interface TrackChange {
  name?: string;
  colour?: number;
  height?: number;
  mix?: TrackMix;
}

export interface ViewChange {
  zoom?: number;
  scroll?: number;
  snap?: Snap;
  library?: boolean;
}

/** Frames per bucket at each level of a file's peaks (engine/src/peaks.rs). */
export const PEAK_LEVELS = [16, 64, 256, 1024, 4096, 16384];

/** Track colours, by index. */
export const COLOURS = ["#8051ff", "#b7ff00", "#ff6b9a", "#4fd1ff", "#ffb36b", "#6bffc8", "#c38bff", "#ffe066", "#ff7a59", "#7aa2ff", "#5fe07a", "#e08bd4"];

export const AUDIO_EXTENSIONS = ["wav", "aif", "aiff", "flac", "mp3", "m4a", "mp4"];

export const state = () => invoke<TimelineView>("timeline_state");
export const addFiles = (files: string[], track: number | null, start: number) => invoke<TimelineView>("timeline_add_files", { files, track, start });
export const edit = (e: Edit) => invoke<TimelineView>("timeline_edit", { edit: e });
/** Splits the clips in `ids` at `beat`, or every clip there when `ids` is empty. */
export const split = (ids: number[], beat: number) => invoke<TimelineView>("timeline_split", { ids, beat });
export const undo = () => invoke<TimelineView>("timeline_undo");
export const redo = () => invoke<TimelineView>("timeline_redo");
export const addTrack = () => invoke<TimelineView>("timeline_track_add");
export const removeTrack = (id: number) => invoke<TimelineView>("timeline_track_remove", { id });
export const setTrack = (id: number, change: TrackChange) => invoke<TimelineView>("timeline_track_set", { id, change });
export const setView = (change: ViewChange) => invoke<TimelineView>("timeline_view", { change });
export const setLoop = (on: boolean, start: number, end: number) => invoke<TimelineView>("timeline_loop", { on, start, end });
export const seek = (beat: number) => invoke<void>("timeline_seek", { beat });
export const play = (playing: boolean) => invoke<void>("timeline_play", { playing });
/** Plays `key` on an instrument track's instrument (`on`), or lets go of it. */
export const previewNote = (track: number, key: number, velocity: number, on: boolean) => invoke<void>("timeline_preview", { track, key, velocity, on });
/** Lets go of every note the keyboard is holding on the track. */
export const previewOff = (track: number) => invoke<void>("timeline_preview", { track, key: null, velocity: null, on: false });
/** A file's peaks at `level`: min then max of each bucket, signed bytes. */
export const peaks = (path: string, level: number) => invoke<ArrayBuffer>("timeline_peaks", { path, level });

/** Sets the tempo the Sampler and the transport share, keeping the Sampler's swing and steps. */
export async function setTempo(bpm: number): Promise<void> {
  const now = await invoke<{ swing: number; steps: number }>("sampler_state");
  await invoke("sampler_transport", { tempo: bpm, swing: now.swing, steps: now.steps });
}

const on =
  <T,>(name: string) =>
  (handler: (payload: T) => void): Promise<UnlistenFn> =>
    listen<T>(name, (e) => handler(e.payload));

export const onState = on<TimelineView>("timeline:state");
export const onTick = on<Tick>("timeline:tick");

/** Runs an unsubscribe once it arrives, even if the component has gone. Outside Tauri, does nothing. */
export function subscribe(p: Promise<UnlistenFn>): () => void {
  let off: UnlistenFn | undefined;
  let gone = false;
  p.then((u) => (gone ? u() : (off = u))).catch(() => {});
  return () => {
    gone = true;
    off?.();
  };
}
