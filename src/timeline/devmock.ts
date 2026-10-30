// Dev only: stands in for the Timeline's Rust side (src-tauri/src/timeline.rs)
// at http://localhost:1420/?mock, on top of the Sampler's mock: it wraps the
// mocked invoke and answers the `timeline_*` commands, and sends ticks while
// "playing". main.tsx loads it only in `npm run dev` builds with `?mock`.
import { emit } from "@tauri-apps/api/event";
import type { ClipIn, ClipView, Edit, SourceView, TimelineView, TrackView } from "./api";
import { clipEnd, isLooped } from "./edits";

type Invoke = (cmd: string, args?: unknown, options?: unknown) => Promise<unknown>;

export function installMocks() {
  const internals = (window as unknown as { __TAURI_INTERNALS__: { invoke: Invoke } }).__TAURI_INTERNALS__;
  const inner = internals.invoke;
  const mix = () => ({ volume: 1, pan: 0, mute: false, solo: false });
  const v: TimelineView = {
    tracks: [{ id: 1, name: "Sampler", kind: "sampler", colour: 0, height: 72, mix: mix(), clips: [] }],
    sources: [],
    tempo: 120,
    signature: [4, 4],
    zoom: 24,
    scroll: 0,
    snap: "beat",
    library: true,
    loopOn: false,
    loopStart: 0,
    loopEnd: 16,
    canUndo: false,
    canRedo: false,
  };
  let nextTrack = 2;
  let nextClip = 1;
  let playing = false;
  let beat = 0;
  let start = 0;
  const undo: string[] = [];
  const redo: string[] = [];
  const snap = () => JSON.stringify({ tracks: v.tracks, sources: v.sources });
  const restore = (s: string) => Object.assign(v, JSON.parse(s));
  const checkpoint = () => {
    undo.push(snap());
    redo.length = 0;
  };
  const publish = () => {
    v.canUndo = undo.length > 0;
    v.canRedo = redo.length > 0;
    const copy = structuredClone(v);
    void emit("timeline:state", copy);
    return copy;
  };
  const name = (p: string) => (p.split("/").pop() ?? p).replace(/\.\w+$/, "");
  const source = (path: string): SourceView => {
    let s = v.sources.find((x) => x.path === path);
    if (!s) {
      const seconds = 0.4 + (path.length % 7) * 0.6;
      s = { path, seconds, channels: 2, frames: Math.round(seconds * 44_100), rate: 44_100, missing: false, version: 1 };
      v.sources.push(s);
    }
    return s;
  };
  const audio = (id: number) => v.tracks.find((t) => t.id === id && t.kind === "audio");
  const newTrack = (n: string): TrackView => {
    const t: TrackView = { id: nextTrack++, name: n, kind: "audio", colour: v.tracks.length % 12, height: 72, mix: mix(), clips: [] };
    v.tracks.push(t);
    return t;
  };
  const toClip = (c: ClipIn, id: number): ClipView => ({ id, path: c.path, name: name(c.path), start: c.start, offset: c.offset, content: c.content, phase: c.phase, length: c.length, fadeIn: c.fadeIn, fadeOut: c.fadeOut, gainDb: c.gainDb });

  const handle = (cmd: string, a: Record<string, unknown>): unknown => {
    switch (cmd) {
      case "timeline_state":
        return publish();
      case "timeline_add_files": {
        checkpoint();
        const files = a.files as string[];
        let at = a.start as number;
        const on = audio((a.track as number | null) ?? -1);
        for (const f of files) {
          const s = source(f);
          const t = on ?? newTrack(name(f));
          t.clips.push({ id: nextClip++, path: f, name: name(f), start: at, offset: 0, content: s.seconds, phase: 0, length: s.seconds, fadeIn: 0, fadeOut: 0, gainDb: 0 });
          if (on) at += s.seconds / (60 / v.tempo);
        }
        return publish();
      }
      case "timeline_edit": {
        const e = a.edit as Edit;
        checkpoint();
        const gone = new Set([...e.delete, ...e.clips.flatMap((c) => (c.id ? [c.id] : []))]);
        for (const t of v.tracks) t.clips = t.clips.filter((c) => !gone.has(c.id));
        const fresh = e.clips.some((c) => c.track === 0) ? newTrack(name(e.clips.find((c) => c.track === 0)!.path)) : null;
        for (const c of e.clips) {
          const t = c.track === 0 ? fresh! : audio(c.track);
          if (!t) throw "the Sampler's track holds the Sampler; put clips on an audio track";
          t.clips.push(toClip(c, c.id ?? nextClip++));
          t.clips.sort((x, y) => x.start - y.start);
        }
        return publish();
      }
      case "timeline_split": {
        const at = a.beat as number;
        const ids = a.ids as number[];
        checkpoint();
        let done = 0;
        for (const t of v.tracks) {
          t.clips = t.clips.flatMap((c) => {
            if ((ids.length && !ids.includes(c.id)) || !(c.start < at && at < clipEnd(c, v.tempo))) return [c];
            done++;
            const n = (at - c.start) * (60 / v.tempo);
            const left = { ...c, length: n, fadeOut: 0, content: isLooped(c) ? c.content : c.phase + n };
            const right = isLooped(c)
              ? { ...c, id: nextClip++, start: at, length: c.length - n, fadeIn: 0, phase: (c.phase + n) % c.content }
              : { ...c, id: nextClip++, start: at, length: c.length - n, fadeIn: 0, offset: c.offset + c.phase + n, content: c.content - c.phase - n, phase: 0 };
            return [left, right];
          });
        }
        if (!done) {
          undo.pop();
          throw "there is no clip to split at the playhead";
        }
        return publish();
      }
      case "timeline_undo":
      case "timeline_redo": {
        const [from, to] = cmd === "timeline_undo" ? [undo, redo] : [redo, undo];
        const s = from.pop();
        if (s) {
          to.push(snap());
          restore(s);
        }
        return publish();
      }
      case "timeline_track_add":
        checkpoint();
        newTrack(`Audio ${v.tracks.length}`);
        return publish();
      case "timeline_track_remove":
        checkpoint();
        v.tracks = v.tracks.filter((t) => t.id !== a.id || t.kind === "sampler");
        return publish();
      case "timeline_track_set": {
        const t = v.tracks.find((x) => x.id === a.id);
        if (!t) throw `there is no track ${a.id}`;
        const c = a.change as Partial<TrackView>;
        if (c.name || c.colour !== undefined || c.mix) checkpoint();
        Object.assign(t, c);
        return publish();
      }
      case "timeline_view":
        Object.assign(v, a.change);
        return structuredClone(v);
      case "timeline_loop":
        Object.assign(v, { loopOn: a.on, loopStart: a.start, loopEnd: a.end });
        return publish();
      case "timeline_seek":
        start = a.beat as number;
        beat = start;
        return null;
      case "timeline_play":
        playing = a.playing as boolean;
        beat = start;
        return null;
      case "timeline_peaks": {
        const s = source(a.path as string);
        const bucket = [16, 64, 256, 1024, 4096, 16384][a.level as number];
        const n = Math.ceil(s.frames / bucket);
        const out = new Int8Array(n * 2);
        for (let i = 0; i < n; i++) {
          const env = Math.exp(-((i * bucket) / s.rate) * 3) * 0.8 + 0.1;
          const x = Math.round(127 * env * (0.6 + 0.4 * Math.abs(Math.sin(i * 0.37))));
          out[2 * i] = -x;
          out[2 * i + 1] = x;
        }
        return out.buffer;
      }
    }
    return undefined;
  };

  internals.invoke = async (cmd, args, options) => {
    if (cmd === "sampler_transport") v.tempo = (args as { tempo: number }).tempo;
    if (cmd.startsWith("timeline_")) return handle(cmd, (args ?? {}) as Record<string, unknown>);
    return inner(cmd, args, options);
  };

  // Like the Rust side, a tick only when something changed. (Tauri's mock never
  // drops a listener that unlistens, so idle ticks would warn about it.)
  let last = performance.now();
  let sent = "";
  setInterval(() => {
    const now = performance.now();
    if (playing) {
      beat += ((now - last) / 1000) * (v.tempo / 60);
      if (v.loopOn && beat >= v.loopEnd) beat = v.loopStart + ((beat - v.loopEnd) % (v.loopEnd - v.loopStart));
    }
    last = now;
    const levels = v.tracks.map((t) => [t.id, playing && t.clips.some((c) => c.start <= beat && beat < clipEnd(c, v.tempo)) ? 0.5 : 0]);
    const tick = { playing, beat: playing ? beat : start, tempo: v.tempo, signature: v.signature, levels };
    const text = JSON.stringify(tick);
    if (text !== sent) void emit("timeline:tick", tick);
    sent = text;
  }, 33);
}
