// Dev only: stands in for the Rust side so the Sampler can be tried in a
// plain browser at http://localhost:1420/?mock. main.tsx loads it only in
// `npm run dev` builds with `?mock` in the URL.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import type { PadView, SamplerView } from "./api";

const peaks = (seed: number) => Array.from({ length: 160 }, (_, i) => Math.max(0, Math.exp(-i / (8 + seed * 3)) * (0.6 + 0.4 * Math.sin(i * seed))));

const LIB = [
  "Drums/Kicks/Kick Tight", "Drums/Kicks/Kick Sub", "Drums/Snares/Snare Crack", "Drums/Hats/Hat Closed",
  "Drums/Hats/Hat Open", "Drums/Perc/Rim", "Vocals/Chop Ah", "Vocals/Chop Oh", "Bass/808 Long", "FX/Riser",
];

export function installMocks() {
  const pad = (index: number): PadView => ({
    index, name: null, path: null, missing: false, seconds: 0, channels: 0, rate: 0, peaks: [],
    settings: { gainDb: 0, pan: 0, pitch: 0, start: 0, end: 1, gate: false, reverse: false, mute: false, choke: 0 },
    steps: Array(16).fill(false),
    velocity: Array(16).fill(1),
  });
  const v: SamplerView = { pads: Array.from({ length: 16 }, (_, i) => pad(i)), tempo: 120, swing: 0, steps: 16, projectName: "Untitled", projectPath: null, dirty: false, canUndo: false, canRedo: false, pattern: 0, patternsUsed: [false, false, false, false], metronome: false, recording: false };
  const played = Array(16).fill(0);
  let playing = false;
  let step = 0;
  const calls: unknown[] = [];
  (window as unknown as { __mockCalls: unknown[] }).__mockCalls = calls;
  const publish = () => {
    const copy = structuredClone(v);
    void emit("sampler:state", copy);
    return copy;
  };
  const resize = () =>
    v.pads.forEach((p) => {
      p.steps = Array.from({ length: v.steps }, (_, i) => p.steps[i] ?? false);
      p.velocity = Array.from({ length: v.steps }, (_, i) => p.velocity[i] ?? 1);
    });

  setInterval(() => {
    if (!playing) return;
    step = (step + 1) % v.steps;
    v.pads.forEach((p) => p.steps[step] && !p.settings.mute && played[p.index]++);
    void emit("sampler:tick", { playing, step, played: [...played] });
  }, 125);

  mockIPC(
    (cmd, args) => {
      const a = args as Record<string, unknown>;
      calls.push({ cmd, args });
      switch (cmd) {
        case "sampler_state":
          return structuredClone(v);
        case "sampler_load": {
          const p = v.pads[a.pad as number];
          const path = a.path as string;
          Object.assign(p, pad(p.index), { name: path.split("/").pop()!.replace(/\.\w+$/, ""), path, seconds: 0.4 + p.index * 0.1, channels: 2, rate: 44100, peaks: peaks(p.index + 1), steps: p.steps });
          v.dirty = true;
          return publish();
        }
        case "sampler_clear":
          v.pads[a.pad as number] = pad(a.pad as number);
          resize();
          v.dirty = true;
          return publish();
        case "sampler_move": {
          const [f, t] = [a.from as number, a.to as number];
          [v.pads[f], v.pads[t]] = [{ ...v.pads[t], index: f }, { ...v.pads[f], index: t }];
          return publish();
        }
        case "sampler_settings":
          v.pads[a.pad as number].settings = a.settings as PadView["settings"];
          v.dirty = true;
          return publish();
        case "sampler_step":
          v.pads[a.pad as number].steps[a.step as number] = a.on as boolean;
          v.dirty = true;
          return publish();
        case "sampler_clear_steps":
          v.pads.forEach((p) => (a.pad === null || a.pad === p.index) && (p.steps = Array(v.steps).fill(false)));
          return publish();
        case "sampler_transport":
          Object.assign(v, { tempo: a.tempo, swing: a.swing, steps: a.steps });
          resize();
          return publish();
        case "sampler_play":
          playing = a.playing as boolean;
          step = -1;
          void emit("sampler:tick", { playing, step: playing ? 0 : null, played: [...played] });
          return null;
        case "sampler_trigger":
          played[a.pad as number]++;
          void emit("sampler:tick", { playing, step: playing ? step : null, played: [...played] });
          return null;
        case "library_default":
          return "/Users/test/Splice/sounds";
        case "library_scan":
          return { root: a.dir, truncated: false, items: LIB.map((n) => ({ path: `${a.dir}/${n}.wav`, name: n.split("/").pop(), folder: n.split("/").slice(0, -1).join("/") })) };
        case "project_new":
          v.pads = v.pads.map((_, i) => pad(i));
          v.dirty = false;
          return publish();
        case "project_save":
          Object.assign(v, { dirty: false, projectName: "Beat 1", projectPath: "/Users/test/Beat 1.tunek" });
          return publish();
        case "list_outputs":
          return [{ id: "coreaudio:built-in", name: "MacBook Pro Speakers", channels: 2, sampleRates: [44100, 48000], preferredRate: 48000, isDefault: true }];
        case "engine_version":
          return "0.1.0";
        case "plugin:dialog|open":
          return "/Users/test/Music/Kick.wav";
        case "plugin:dialog|save":
          return "/Users/test/Music/Beat 1.tunek";
        case "plugin:dialog|ask":
          return true;
        case "sampler_bounce": {
          const free = v.pads.find((p) => !p.name);
          if (!free) throw "every pad is in use";
          Object.assign(free, { name: "Untitled bounce 1", path: "/Users/test/Music/tuneKore/Samples/Bounces/Untitled bounce 1.wav", seconds: 8, channels: 2, rate: 48000, peaks: peaks(3) });
          publish();
          return { pad: free.index, path: free.path };
        }
        case "sampler_velocity":
          v.pads[a.pad as number].velocity[a.step as number] = a.velocity as number;
          return publish();
        case "sampler_pattern":
          v.pattern = a.index as number;
          return publish();
        case "sampler_copy_pattern":
          v.pattern = a.to as number;
          return publish();
        case "sampler_metronome":
          v.metronome = a.on as boolean;
          return publish();
        case "sampler_record":
          v.recording = a.on as boolean;
          return publish();
        case "sampler_chop":
          return { pads: [1, 2, 3, 4], folder: "/Users/test/Music/tuneKore/Samples/Chops/Kick chop 1" };
        case "sampler_undo":
        case "sampler_redo":
          return publish();
        case "sampler_export":
          return { path: a.path, seconds: 8, format: "pcm24" };
        default:
          return null;
      }
    },
    { shouldMockEvents: true },
  );
  mockWindows("main");
}
