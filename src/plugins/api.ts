// Typed wrappers for the plugins' commands and events (src-tauri/src/plugins/).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export { subscribe } from "../timeline/api";

export type Role = "instrument" | "effect" | "pad";
export type Kind = "instrument" | "effect" | "other";

/** A plugin on a track. */
export interface DeviceView {
  id: number;
  track: number;
  role: Role;
  /** The pad a "pad" device plays, from 0. */
  pad: number;
  name: string;
  vendor: string;
  /** "missing": it didn't load (its state is kept); "failed": it threw while playing and is off. */
  status: "ok" | "missing" | "failed";
  /** Why it is missing or failed. */
  error: string | null;
  /** What needs a look on one that loaded: its saved sound didn't load, its sound wasn't saved, it couldn't start. */
  warning: string | null;
  /** Its saved sound didn't load: keepSound takes the sound it plays now. */
  soundLost: boolean;
  bypass: boolean;
  hasEditor: boolean;
  editorOpen: boolean;
  latency: number;
}

export interface PluginsView {
  devices: DeviceView[];
  /** The track whose instrument a MIDI controller's keys play, or null (the pads). */
  focus: number | null;
}

/** One plugin the scanner found. */
export interface CatalogEntry {
  path: string;
  classId: string;
  name: string;
  vendor: string;
  version: string;
  kind: Kind;
  subCategories: string[];
}

export interface FailedEntry {
  name: string;
  path: string;
  status: "ok" | "failed" | "crashed" | "timed_out";
  error: string | null;
}

export interface CatalogView {
  plugins: CatalogEntry[];
  failed: FailedEntry[];
  scanning: boolean;
  error: string | null;
}

export type ScanMode = "update" | "rescan" | "retry";

export const state = () => invoke<PluginsView>("plugins_state");
export const catalog = () => invoke<CatalogView>("plugins_catalog");
export const scan = (mode: ScanMode) => invoke<void>("plugins_scan", { mode });
/**
 * Adds a plugin: on `pad` (from 0) when given; else on `track` (an effect at
 * `index`, the end by default); else an instrument on a new instrument track.
 */
export const add = (plugin: CatalogEntry, track: number | null, index?: number, pad?: number) =>
  invoke<PluginsView>("plugins_add", { plugin: { path: plugin.path, classId: plugin.classId }, track, index: index ?? null, pad: pad ?? null });
export const remove = (id: number) => invoke<PluginsView>("plugins_remove", { id });
export const removeTrack = (track: number) => invoke<PluginsView>("plugins_remove_track", { track });
export const bypass = (id: number, on: boolean) => invoke<PluginsView>("plugins_bypass", { id, on });
export const move = (id: number, to: number) => invoke<PluginsView>("plugins_move", { id, to });
export const editor = (id: number, open: boolean) => invoke<PluginsView>("plugins_editor", { id, open });
export const reload = (id: number) => invoke<PluginsView>("plugins_reload", { id });
/** After its saved sound didn't load: saving writes the sound it plays now. */
export const keepSound = (id: number) => invoke<PluginsView>("plugins_keep_sound", { id });
export const focus = (track: number | null) => invoke<PluginsView>("plugins_focus", { track });

const on =
  <T,>(name: string) =>
  (handler: (payload: T) => void): Promise<UnlistenFn> =>
    listen<T>(name, (e) => handler(e.payload));

export const onState = on<PluginsView>("plugins:state");
export const onCatalog = on<CatalogView>("plugins:catalog");
/** Plugins a project named that didn't come back as saved when it opened (missing, or their sound didn't load). */
export const onMissing = on<string[]>("plugins:missing");
/** Plugins that wouldn't give their sound at a save: the project kept the sound they had. Sent once until a save works. */
export const onUnsaved = on<string[]>("plugins:unsaved");

/**
 * Runs `f` once the window has painted what was just set (a "Loading…"
 * message), or after 100 ms if it doesn't paint (a hidden window). Loading a
 * plugin holds the UI's thread, so the message must be on screen first.
 */
export function afterPaint(f: () => void) {
  let done = false;
  const go = () => {
    if (done) return;
    done = true;
    f();
  };
  requestAnimationFrame(() => requestAnimationFrame(go));
  setTimeout(go, 100);
}

/** The plugin a pad plays, if any. */
export const padPlugin = (v: PluginsView | null, pad: number) => v?.devices.find((d) => d.role === "pad" && d.pad === pad) ?? null;

/** A MIDI note's name as tuneKore shows them, C3 being 60. */
export function noteName(n: number): string {
  const names = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];
  const k = Math.max(0, Math.min(127, Math.round(n)));
  return `${names[k % 12]}${Math.floor(k / 12) - 2}`;
}

/** A track's devices: its instrument, then its effects in chain order. */
export function chainOf(v: PluginsView | null, track: number): { instrument: DeviceView | null; effects: DeviceView[] } {
  const mine = (v?.devices ?? []).filter((d) => d.track === track);
  return { instrument: mine.find((d) => d.role === "instrument") ?? null, effects: mine.filter((d) => d.role === "effect") };
}
