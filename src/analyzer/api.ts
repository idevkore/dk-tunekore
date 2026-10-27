// Typed wrappers for the Analyzer's commands and events (src-tauri/src/analyzer.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface Backend {
  id: string;
  name: string;
  modes: string[];
  modeLabels: Record<string, string>;
  available: boolean;
  reason: string | null;
}

export interface SongInfo {
  name: string;
  path: string;
  seconds: number;
  sampleRate: number;
  channels: number;
  codec: string;
  bits: number | null;
  peaks: number[];
}

export interface StemInfo {
  name: string;
  peaks: number[];
  rmsDb: number;
}

export interface Checks {
  sameLength: boolean;
  sumSnrDb: number;
  lagFrames: number;
}

export interface Progress {
  job: number;
  stage: string;
  fraction: number;
}

export interface Finished {
  job: number;
  backend: string;
  mode: string;
  stems: StemInfo[];
  seconds: number;
  peakRssMb: number;
  peakGpuMb: number;
  checks: Checks;
}

export interface Failed {
  job: number;
  message: string;
  cancelled: boolean;
}

export interface Position {
  position: number;
  duration: number;
  playing: boolean;
}

export interface Exported {
  path: string;
  format: "pcm24" | "float32";
}

export const SONG_EXTENSIONS = ["wav", "aif", "aiff", "flac", "mp3", "m4a", "mp4"];

export const openAnalyzer = () => invoke<void>("open_analyzer");
export const backends = () => invoke<Backend[]>("analyzer_backends");
export const openSong = (path: string) => invoke<SongInfo>("analyzer_open", { path });
export const separate = (backend: string, mode: string) => invoke<number>("analyzer_separate", { backend, mode });
export const cancel = () => invoke<void>("analyzer_cancel");
export const play = (playing: boolean) => invoke<void>("analyzer_play", { playing });
export const seek = (seconds: number) => invoke<void>("analyzer_seek", { seconds });
export const setTrack = (index: number, gain: number, mute: boolean, solo: boolean) =>
  invoke<void>("analyzer_track", { index, gain, mute, solo });
export const exportStems = (dir: string, names: string[]) => invoke<Exported[]>("analyzer_export", { dir, names });

export interface ToPads {
  pads: number[];
  folder: string;
}

/** Puts a stem on the Sampler's pads: `how` is "whole" or "chop". */
export const toPads = (stem: string, how: string, count: number, firstPad: number | null, fromSeconds: number) =>
  invoke<ToPads>("analyzer_to_pads", { stem, how, count, firstPad, fromSeconds });

const on =
  <T,>(name: string) =>
  (handler: (payload: T) => void): Promise<UnlistenFn> =>
    listen<T>(name, (e) => handler(e.payload));

export const onProgress = on<Progress>("analyzer:progress");
export const onDone = on<Finished>("analyzer:done");
export const onFailed = on<Failed>("analyzer:failed");
export const onPosition = on<Position>("analyzer:position");
