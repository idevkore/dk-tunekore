// Waveform peaks, fetched from the Rust side as bytes (timeline_peaks) one
// level of one file at a time, and kept. Until a level arrives the nearest
// one already here stands in; when it lands, listeners redraw.
import { peaks as fetchPeaks, PEAK_LEVELS } from "./api";

type Entry = Int8Array | "loading" | "failed";

const cache = new Map<string, Entry[]>();
const listeners = new Set<() => void>();

/** A level's data: `[min, max]` pairs, `bucket` frames each. */
export interface Peaks {
  data: Int8Array;
  bucket: number;
}

/** The level whose buckets are no wider than `framesPerPixel`, so each pixel has at least one. */
export function levelFor(framesPerPixel: number): number {
  let level = 0;
  for (let l = 0; l < PEAK_LEVELS.length; l++) if (PEAK_LEVELS[l] <= framesPerPixel) level = l;
  return level;
}

/** The peaks of `path` at `level`, or the nearest level already loaded; fetches the one asked for. */
export function getPeaks(path: string, level: number): Peaks | null {
  let e = cache.get(path);
  if (!e) {
    e = [];
    cache.set(path, e);
  }
  const want = e[level];
  if (want instanceof Int8Array) return { data: want, bucket: PEAK_LEVELS[level] };
  if (want === undefined) {
    const entries = e;
    entries[level] = "loading";
    fetchPeaks(path, level)
      .then((buf) => {
        entries[level] = new Int8Array(buf);
        listeners.forEach((f) => f());
      })
      .catch(() => {
        entries[level] = "failed";
      });
  }
  for (let d = 1; d < PEAK_LEVELS.length; d++) {
    for (const l of [level + d, level - d]) {
      const v = e[l];
      if (l >= 0 && l < PEAK_LEVELS.length && v instanceof Int8Array) return { data: v, bucket: PEAK_LEVELS[l] };
    }
  }
  return null;
}

/** Forgets a file's peaks that couldn't be fetched (it wasn't decoded yet), so they are asked for again. */
export function retryFailed() {
  cache.forEach((e) => e.forEach((v, i) => v === "failed" && delete e[i]));
}

/** Calls `f` whenever a level arrives. Returns the unsubscribe. */
export function onPeaks(f: () => void): () => void {
  listeners.add(f);
  return () => listeners.delete(f);
}
