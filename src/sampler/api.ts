// Typed wrappers for the Sampler's commands and events (src-tauri/src/sampler.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const PADS = 16;
/** The computer keys that play pads 1–16, laid out like the 4×4 grid. */
export const PAD_KEYS = ["1", "2", "3", "4", "q", "w", "e", "r", "a", "s", "d", "f", "z", "x", "c", "v"];
export const STEP_CHOICES = [8, 16, 32];
/** Project files: `Beat 1.tunek`. */
export const PROJECT_EXT = "tunek";
export const AUDIO_EXTENSIONS = ["wav", "aif", "aiff", "flac", "mp3", "m4a", "mp4"];

export interface PadSettings {
  /** -60 (off) to +6. */
  gainDb: number;
  /** -1 left to 1 right. */
  pan: number;
  /** Semitones, -24..24. */
  pitch: number;
  /** Fractions of the sample, 0..1. */
  start: number;
  end: number;
  gate: boolean;
  reverse: boolean;
  mute: boolean;
  /** 0 = none; pads in the same group 1–8 cut each other off. */
  choke: number;
  /** Amp envelope: seconds, seconds, level 0..1, seconds. */
  attack: number;
  decay: number;
  sustain: number;
  release: number;
  /** 0 off, 1 low-pass, 2 high-pass, 3 band-pass. */
  filter: number;
  /** Hz, 20..20,000. */
  cutoff: number;
  /** 0..1 */
  resonance: number;
}

export interface PadView {
  index: number;
  name: string | null;
  path: string | null;
  missing: boolean;
  seconds: number;
  channels: number;
  rate: number;
  peaks: number[];
  settings: PadSettings;
  /** One per step of the pattern. */
  steps: boolean[];
  /** How hard each step plays, 0..1, one per step. */
  velocity: number[];
}

export interface SamplerView {
  pads: PadView[];
  tempo: number;
  /** 0..1 */
  swing: number;
  steps: number;
  projectName: string;
  projectPath: string | null;
  dirty: boolean;
  canUndo: boolean;
  canRedo: boolean;
  /** The pattern playing and being edited, 0 = A. */
  pattern: number;
  patternsUsed: boolean[];
  metronome: boolean;
  recording: boolean;
}

export interface Tick {
  playing: boolean;
  step: number | null;
  /** Play count per pad; a change means the pad just played. */
  played: number[];
}

export interface LibraryItem {
  path: string;
  name: string;
  folder: string;
}

export interface LibraryScan {
  root: string;
  items: LibraryItem[];
  truncated: boolean;
}

export interface Exported {
  path: string;
  seconds: number;
  format: "pcm24" | "float32";
}

export const state = () => invoke<SamplerView>("sampler_state");
export const loadPad = (pad: number, path: string) => invoke<SamplerView>("sampler_load", { pad, path });
export const clearPad = (pad: number) => invoke<SamplerView>("sampler_clear", { pad });
export const movePad = (from: number, to: number) => invoke<SamplerView>("sampler_move", { from, to });
export const setSettings = (pad: number, settings: PadSettings) => invoke<SamplerView>("sampler_settings", { pad, settings });
export const setStep = (pad: number, step: number, on: boolean) => invoke<SamplerView>("sampler_step", { pad, step, on });
export const clearSteps = (pad: number | null) => invoke<SamplerView>("sampler_clear_steps", { pad });
export const setTransport = (tempo: number, swing: number, steps: number) =>
  invoke<SamplerView>("sampler_transport", { tempo, swing, steps });
export const play = (playing: boolean) => invoke<void>("sampler_play", { playing });
export const trigger = (pad: number) => invoke<void>("sampler_trigger", { pad });
export const release = (pad: number) => invoke<void>("sampler_release", { pad });
export const PATTERN_NAMES = ["A", "B", "C", "D"];
export const setVelocity = (pad: number, step: number, velocity: number) => invoke<SamplerView>("sampler_velocity", { pad, step, velocity });
export const selectPattern = (index: number) => invoke<SamplerView>("sampler_pattern", { index });
export const copyPattern = (from: number, to: number) => invoke<SamplerView>("sampler_copy_pattern", { from, to });
export const setMetronome = (on: boolean) => invoke<SamplerView>("sampler_metronome", { on });
export const setRecording = (on: boolean) => invoke<SamplerView>("sampler_record", { on });
export const chop = (pad: number, count: number) => invoke<{ pads: number[]; folder: string }>("sampler_chop", { pad, count });
export const exportStems = (dir: string, loops: number) => invoke<Exported[]>("sampler_export_stems", { dir, loops });
export const collectSamples = () => invoke<SamplerView>("project_collect");
export const undo = () => invoke<SamplerView>("sampler_undo");
export const redo = () => invoke<SamplerView>("sampler_redo");
export const bounce = (loops: number) => invoke<{ pad: number; path: string }>("sampler_bounce", { loops });
export const exportPattern = (path: string, loops: number) => invoke<Exported>("sampler_export", { path, loops });

export const scanLibrary = (dir: string) => invoke<LibraryScan>("library_scan", { dir });
export const defaultLibrary = () => invoke<string | null>("library_default");
export const preview = (path: string) => invoke<void>("library_preview", { path });
export const stopPreview = () => invoke<void>("library_stop");

export const newProject = () => invoke<SamplerView>("project_new");
export const openProject = (path: string) => invoke<SamplerView>("project_open", { path });
export const saveProject = (path: string | null) => invoke<SamplerView>("project_save", { path });

export const onState = (handler: (v: SamplerView) => void): Promise<UnlistenFn> =>
  listen<SamplerView>("sampler:state", (e) => handler(e.payload));
export const onTick = (handler: (t: Tick) => void): Promise<UnlistenFn> =>
  listen<Tick>("sampler:tick", (e) => handler(e.payload));

/** The Analyzer wrote samples into `folder` and put them on pads. */
export const onChopped = (handler: (folder: string) => void): Promise<UnlistenFn> =>
  listen<string>("sampler:chopped", (e) => handler(e.payload));

/** The window is closing or the app quitting with unsaved changes: ask, then call `quitApp`. */
export const onConfirmQuit = (handler: () => void): Promise<UnlistenFn> => listen("app:confirm-quit", () => handler());
export const quitApp = () => invoke<void>("app_quit");

export interface Recovery {
  projectName: string;
  projectPath: string | null;
  /** Unix seconds. */
  savedAt: number;
}
export const autosaveCheck = () => invoke<Recovery | null>("autosave_check");
export const autosaveRecover = () => invoke<SamplerView>("autosave_recover");
export const autosaveDiscard = () => invoke<void>("autosave_discard");

export const currentOutput = () => invoke<string | null | undefined>("current_output");
export const setOutput = (deviceId: string | null) => invoke<void>("set_output", { deviceId });

/** Subscribes to a Tauri event from a React effect; returns the cleanup. */
export function subscribe(start: Promise<UnlistenFn>): () => void {
  let off: UnlistenFn | undefined;
  let gone = false;
  start.then((u) => (gone ? u() : (off = u))).catch(() => {});
  return () => {
    gone = true;
    off?.();
  };
}
