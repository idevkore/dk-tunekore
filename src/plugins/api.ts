// Typed wrappers for the plugins' commands and events (src-tauri/src/plugins/).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export { subscribe } from "../timeline/api";

export type Role = "instrument" | "effect";
export type Kind = "instrument" | "effect" | "other";

/** A plugin on a track. */
export interface DeviceView {
  id: number;
  track: number;
  role: Role;
  name: string;
  vendor: string;
  /** "missing": it didn't load (its state is kept); "failed": it threw while playing and is off. */
  status: "ok" | "missing" | "failed";
  error: string | null;
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
/** An instrument with no track makes a new instrument track; an effect goes at `index` (the end by default). */
export const add = (plugin: CatalogEntry, track: number | null, index?: number) =>
  invoke<PluginsView>("plugins_add", { plugin: { path: plugin.path, classId: plugin.classId }, track, index: index ?? null });
export const remove = (id: number) => invoke<PluginsView>("plugins_remove", { id });
export const removeTrack = (track: number) => invoke<PluginsView>("plugins_remove_track", { track });
export const bypass = (id: number, on: boolean) => invoke<PluginsView>("plugins_bypass", { id, on });
export const move = (id: number, to: number) => invoke<PluginsView>("plugins_move", { id, to });
export const editor = (id: number, open: boolean) => invoke<PluginsView>("plugins_editor", { id, open });
export const reload = (id: number) => invoke<PluginsView>("plugins_reload", { id });
export const focus = (track: number | null) => invoke<PluginsView>("plugins_focus", { track });

const on =
  <T,>(name: string) =>
  (handler: (payload: T) => void): Promise<UnlistenFn> =>
    listen<T>(name, (e) => handler(e.payload));

export const onState = on<PluginsView>("plugins:state");
export const onCatalog = on<CatalogView>("plugins:catalog");
/** Plugins a project named that didn't load when it opened. */
export const onMissing = on<string[]>("plugins:missing");

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

/** A track's devices: its instrument, then its effects in chain order. */
export function chainOf(v: PluginsView | null, track: number): { instrument: DeviceView | null; effects: DeviceView[] } {
  const mine = (v?.devices ?? []).filter((d) => d.track === track);
  return { instrument: mine.find((d) => d.role === "instrument") ?? null, effects: mine.filter((d) => d.role === "effect") };
}
