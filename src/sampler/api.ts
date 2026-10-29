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
  /** How likely each step is to play, 0..1. */
  chance: number[];
  /** Hits per step, 1–4. */
  ratchet: number[];
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
  /** The pattern being edited, 0 = A. Outside song mode it also plays, from
   * the next switch; `Tick.playingPattern` is the one playing now. */
  pattern: number;
  patternsUsed: boolean[];
  metronome: boolean;
  recording: boolean;
  /** Song mode: patterns to play in order (0 = A). */
  chain: number[];
  songMode: boolean;
  songLoop: boolean;
  /** When a switch made while playing lands: at the end of the pattern, or on the next step. */
  patternSwitch: PatternSwitch;
}

export type PatternSwitch = "end" | "now";

export interface Tick {
  playing: boolean;
  step: number | null;
  /** Play count per pad; a change means the pad just played. */
  played: number[];
  /**
   * Which chain entry is playing. A song starts at the end of the pattern, and
   * after song mode is turned off its entry plays to the end, so this can lag
   * the view's `songMode`.
   */
  chainPosition: number | null;
  /** The pattern playing now; while a switch waits it differs from the view's `pattern`. */
  playingPattern: number | null;
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
export const setPatternSwitch = (mode: PatternSwitch) => invoke<SamplerView>("sampler_switch", { mode });
export const copyPattern = (from: number, to: number) => invoke<SamplerView>("sampler_copy_pattern", { from, to });
export const setMetronome = (on: boolean) => invoke<SamplerView>("sampler_metronome", { on });
export const setRecording = (on: boolean) => invoke<SamplerView>("sampler_record", { on });
export const chop = (pad: number, count: number) => invoke<{ pads: number[]; folder: string }>("sampler_chop", { pad, count });
export const exportStems = (dir: string, loops: number, song: boolean) => invoke<Exported[]>("sampler_export_stems", { dir, loops, song });
export const setStepExtras = (pad: number, step: number, chance: number, ratchet: number) =>
  invoke<SamplerView>("sampler_step_extras", { pad, step, chance, ratchet });
export const setSong = (chain: number[], songMode: boolean, songLoop: boolean) => invoke<SamplerView>("sampler_song", { chain, songMode, songLoop });
export const collectSamples = () => invoke<SamplerView>("project_collect");
export const undo = () => invoke<SamplerView>("sampler_undo");
export const redo = () => invoke<SamplerView>("sampler_redo");
export const bounce = (loops: number, song: boolean) => invoke<{ pad: number; path: string }>("sampler_bounce", { loops, song });
export const exportPattern = (path: string, loops: number, song: boolean) => invoke<Exported>("sampler_export", { path, loops, song });

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

// MIDI controllers (src-tauri/src/midi.rs): notes 36–51 play pads 1–16.
export interface MidiInputInfo {
  name: string;
  connected: boolean;
}
export const midiInputs = () => invoke<MidiInputInfo[]>("midi_inputs");
/** A pad was played from a controller; at most ~30 a second. */
export const onMidiActivity = (handler: (pad: number) => void): Promise<UnlistenFn> =>
  listen<number>("midi:activity", (e) => handler(e.payload));

// Sampling from an audio input (src-tauri/src/recorder.rs).
export interface InputDevice {
  id: string;
  name: string;
  channels: number;
  preferredRate: number;
  isDefault: boolean;
}
export interface InputLevel {
  /** 0..1, a little over 1 when the input clips. */
  peak: number;
  seconds: number;
  /** The take hit its 10-minute cap. */
  full: boolean;
}
export const inputDevices = () => invoke<InputDevice[]>("input_devices");
export const samplingStart = (pad: number, deviceId: string | null) => invoke<void>("sampling_start", { pad, deviceId });
export const samplingStop = () => invoke<{ pad: number; path: string; seconds: number }>("sampling_stop");
export const samplingCancel = () => invoke<void>("sampling_cancel");
export const onInputLevel = (handler: (l: InputLevel) => void): Promise<UnlistenFn> =>
  listen<InputLevel>("input:level", (e) => handler(e.payload));
/** The input went away or stopped while recording; the take can't go on. */
export const onInputError = (handler: (message: string) => void): Promise<UnlistenFn> =>
  listen<string>("input:error", (e) => handler(e.payload));

/** The input device to sample from ("" = the system default), kept between runs. */
export const INPUT_DEVICE_KEY = "tunekore.inputDevice";
export function chosenInput(): string | null {
  try {
    return localStorage.getItem(INPUT_DEVICE_KEY) || null;
  } catch {
    return null;
  }
}
export function chooseInput(id: string | null) {
  try {
    if (id) localStorage.setItem(INPUT_DEVICE_KEY, id);
    else localStorage.removeItem(INPUT_DEVICE_KEY);
  } catch {
    // no storage: the choice lasts until the window closes
  }
}

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
