// Typed wrappers for project export's commands and events (src-tauri/src/export.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Range = "song" | "loop";
export type Format = "pcm16" | "pcm24" | "float32";

export interface TrackOut {
  id: number;
  name: string;
  /** Not muted, and soloed when anything is: it gets a stem. */
  heard: boolean;
}

/** What the dialog shows before an export. */
export interface ExportInfo {
  project: string;
  /** The files' rate: the rate the tracks run at. */
  rate: number;
  tempo: number;
  /** Beats per bar and the note value of a beat: [4, 4]. */
  signature: [number, number];
  /** The song's end in beats, or null when nothing sets one. */
  songEnd: number | null;
  songSeconds: number;
  loopStart: number;
  loopEnd: number;
  loopOn: boolean;
  loopSeconds: number;
  /** The longest tail. */
  tailSeconds: number;
  tracks: TrackOut[];
  /** An export is running already. */
  busy: boolean;
}

export interface ExportRequest {
  range: Range;
  format: Format;
  master: boolean;
  stems: boolean;
  tail: boolean;
  /** The master's file; with stems only, the folder they go in. */
  path: string;
}

export interface ExportProgress {
  phase: "settling" | "playing" | "tail";
  /** Seconds written, of the range's. */
  seconds: number;
  range: number;
}

export interface ExportDone {
  files: string[];
  /** Seconds in each file. */
  seconds: number;
  /** How long the export took. */
  took: number;
  /** Files an integer format clipped. */
  clipped: string[];
}

export const info = () => invoke<ExportInfo>("export_info");
export const start = (request: ExportRequest) => invoke<void>("export_start", { request });
export const cancel = () => invoke<void>("export_cancel");

export const onOpen = (f: () => void): Promise<UnlistenFn> => listen("export:open", () => f());
export const onProgress = (f: (p: ExportProgress) => void): Promise<UnlistenFn> => listen<ExportProgress>("export:progress", (e) => f(e.payload));
export const onDone = (f: (d: ExportDone) => void): Promise<UnlistenFn> => listen<ExportDone>("export:done", (e) => f(e.payload));
/** Cancelled (by the user, or the output closing), with what to tell them. */
export const onCancelled = (f: (why: string) => void): Promise<UnlistenFn> => listen<string>("export:cancelled", (e) => f(e.payload));
export const onFailed = (f: (error: string) => void): Promise<UnlistenFn> => listen<string>("export:failed", (e) => f(e.payload));

/** m:ss, or m:ss.s under ten seconds. */
export function duration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  if (seconds < 10) return `0:0${seconds.toFixed(1)}`;
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}:${String(s).padStart(2, "0")}`;
}

/** A position in beats (quarter notes) as bar.beat, both from 1: in 4/4, beat 4 is "2.1". */
export function bars(beats: number, [num, den]: [number, number]): string {
  const perBar = (num * 4) / den;
  const beat = 4 / den;
  const bar = Math.floor(beats / perBar + 1e-9);
  const inBar = Math.floor((beats - bar * perBar) / beat + 1e-9);
  return `${bar + 1}.${inBar + 1}`;
}
