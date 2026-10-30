// MIDI learn: typed wrappers for the mapping commands and events
// (src-tauri/src/midi.rs, its rules in src-tauri/src/midi/map.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { PadSettings } from "../sampler/api";

/** A controller: a control change on a channel, or a 14-bit pair (`cc` and `cc + 32`). */
export interface Source {
  /** 1–16. */
  channel: number;
  cc: number;
  bits: 7 | 14;
}

export interface MappingView {
  /** The control's id, e.g. `sampler.pad.selected.cutoff`. */
  target: string;
  /** "Cutoff"; the id when the control isn't there now. */
  label: string;
  /** "Selected pad", "Pad 3", "Transport", "Output", "Track 1 · Sampler". */
  group: string;
  source: Source;
  /** "Ch 1 · CC 74". */
  controller: string;
  /** False for a control that isn't in this project now (a removed plugin's). */
  available: boolean;
}

/** What a knob does when it and its control disagree. */
export type Takeover = "pickup" | "jump" | "scale";

export interface MidiView {
  mappings: MappingView[];
  /** The control waiting for a knob, if any. */
  learning: string | null;
  takeover: Takeover;
}

export interface TargetInfo {
  id: string;
  label: string;
  group: string;
}

export const midiMappings = () => invoke<MidiView>("midi_mappings");
export const midiTargets = () => invoke<TargetInfo[]>("midi_targets");
/** Learns `target` from the next knob or fader moved; `null` stops learning. */
export const midiLearn = (target: string | null) => invoke<MidiView>("midi_learn", { target });
export const midiForget = (target: string) => invoke<MidiView>("midi_forget", { target });
/** Which pad `sampler.pad.selected.*` moves. */
export const midiFocus = (pad: number) => invoke<void>("midi_focus", { pad });
export const midiTakeover = (mode: Takeover) => invoke<MidiView>("midi_takeover", { mode });

export const onMappings = (handler: (v: MidiView) => void): Promise<UnlistenFn> =>
  listen<MidiView>("midi:mappings", (e) => handler(e.payload));
/** A knob was learned. */
export const onLearned = (handler: (m: MappingView) => void): Promise<UnlistenFn> =>
  listen<MappingView>("midi:learned", (e) => handler(e.payload));
/** Controls a knob moved, at most ~30 times a second, with their values in their own units. */
export const onMoved = (handler: (moved: { target: string; value: number }[]) => void): Promise<UnlistenFn> =>
  listen<{ target: string; value: number }[]>("midi:moved", (e) => handler(e.payload));

export const TEMPO = "transport.tempo";
export const SWING = "sampler.swing";
export const MASTER = "master.level";

/** The pad settings a knob can move: each one's id part and name (as in src-tauri/src/midi/targets.rs). */
export const PAD_CONTROLS: Partial<Record<keyof PadSettings, { name: string; label: string }>> = {
  gainDb: { name: "level", label: "Level" },
  pan: { name: "pan", label: "Pan" },
  pitch: { name: "pitch", label: "Pitch" },
  start: { name: "start", label: "Start" },
  end: { name: "end", label: "End" },
  cutoff: { name: "cutoff", label: "Cutoff" },
  resonance: { name: "resonance", label: "Resonance" },
  attack: { name: "attack", label: "Attack" },
  decay: { name: "decay", label: "Decay" },
  sustain: { name: "sustain", label: "Sustain" },
  release: { name: "release", label: "Release" },
  fenvAmount: { name: "fenvAmount", label: "Filter env amount" },
  fenvAttack: { name: "fenvAttack", label: "Filter env attack" },
  fenvDecay: { name: "fenvDecay", label: "Filter env decay" },
  fenvSustain: { name: "fenvSustain", label: "Filter env sustain" },
  fenvRelease: { name: "fenvRelease", label: "Filter env release" },
  velCutoff: { name: "velCutoff", label: "Velocity to cutoff" },
  velTime: { name: "velTime", label: "Velocity to time" },
};

/** `sampler.pad.selected.cutoff`, or `sampler.pad.3.cutoff` for pad index 2. */
export function padTarget(pad: number | "selected", setting: keyof PadSettings): string {
  return `sampler.pad.${pad === "selected" ? "selected" : pad + 1}.${PAD_CONTROLS[setting]?.name ?? setting}`;
}

/** What a selected-pad slider learns, for the Inspector: its id and name. */
export function padLearn(setting: keyof PadSettings): { target: string; learnAs: string } {
  return { target: padTarget("selected", setting), learnAs: PAD_CONTROLS[setting]?.label ?? setting };
}

/** The pad-only id for a selected-pad id, given the pad selected now. */
export function pinned(target: string, selected: number): string | null {
  const prefix = "sampler.pad.selected.";
  return target.startsWith(prefix) ? `sampler.pad.${selected + 1}.${target.slice(prefix.length)}` : null;
}

/** "CC 74", or "CC 7+39". */
export const shortController = (s: Source) => (s.bits === 14 ? `CC ${s.cc}+${s.cc + 32}` : `CC ${s.cc}`);

/** The takeover mode, kept on this Mac like the input device. */
const TAKEOVER_KEY = "tunekore.midiTakeover";
export function chosenTakeover(): Takeover | null {
  try {
    const v = localStorage.getItem(TAKEOVER_KEY);
    return v === "pickup" || v === "jump" || v === "scale" ? v : null;
  } catch {
    return null;
  }
}
export function chooseTakeover(mode: Takeover) {
  try {
    localStorage.setItem(TAKEOVER_KEY, mode);
  } catch {
    // no storage: the choice lasts until the window closes
  }
}
