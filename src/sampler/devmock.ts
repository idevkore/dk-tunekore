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
    settings: { gainDb: 0, pan: 0, pitch: 0, start: 0, end: 1, gate: false, reverse: false, mute: false, choke: 0, attack: 0, decay: 0, sustain: 1, release: 0.004, filter: 0, cutoff: 20000, resonance: 0, fenvAmount: 0, fenvAttack: 0, fenvDecay: 0.3, fenvSustain: 0, fenvRelease: 0.1, velCutoff: 0, velTime: 0 },
    steps: Array(16).fill(false),
    velocity: Array(16).fill(1),
    chance: Array(16).fill(1),
    ratchet: Array(16).fill(1),
  });
  const v: SamplerView = { pads: Array.from({ length: 16 }, (_, i) => pad(i)), tempo: 120, swing: 0, steps: 16, projectName: "Untitled", projectPath: null, dirty: false, canUndo: false, canRedo: false, pattern: 0, patternsUsed: [false, false, false, false], metronome: false, recording: false, chain: [], songMode: false, songLoop: true, patternSwitch: "end" };
  const played = Array(16).fill(0);
  let playing = false;
  let step = 0;
  // The pattern playing: like the engine, a switch waits for the wrap unless it's set to now.
  let live = 0;
  const tick = () => ({ playing, step: playing ? step : null, played: [...played], chainPosition: null, playingPattern: playing ? live : null });
  let sampling: { pad: number; started: number } | null = null;
  let sampler: ReturnType<typeof setInterval> | undefined;
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
      p.chance = Array.from({ length: v.steps }, (_, i) => p.chance[i] ?? 1);
      p.ratchet = Array.from({ length: v.steps }, (_, i) => p.ratchet[i] ?? 1);
    });

  setInterval(() => {
    if (!playing) return;
    step = (step + 1) % v.steps;
    if (step === 0 || v.patternSwitch === "now") live = v.pattern;
    v.pads.forEach((p) => p.steps[step] && !p.settings.mute && played[p.index]++);
    void emit("sampler:tick", tick());
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
          step = playing ? 0 : -1;
          live = v.pattern;
          void emit("sampler:tick", tick());
          step = -1;
          return null;
        case "sampler_trigger":
          played[a.pad as number]++;
          void emit("sampler:tick", tick());
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
        case "midi_inputs":
          return [{ name: "Mock Pad Controller", connected: true }];
        case "input_devices":
          return [{ id: "coreaudio:mock-mic", name: "MacBook Pro Microphone", channels: 1, preferredRate: 48000, isDefault: true }];
        case "sampling_start": {
          sampling = { pad: a.pad as number, started: Date.now() };
          sampler = setInterval(() => {
            const seconds = (Date.now() - sampling!.started) / 1000;
            void emit("input:level", { peak: 0.5 + 0.4 * Math.sin(seconds * 6), seconds, full: false });
          }, 33);
          return null;
        }
        case "sampling_cancel":
          clearInterval(sampler);
          sampling = null;
          return null;
        case "sampling_stop": {
          clearInterval(sampler);
          const p = v.pads[sampling!.pad];
          const seconds = Math.max(0.2, (Date.now() - sampling!.started) / 1000);
          sampling = null;
          Object.assign(p, pad(p.index), { name: "Recording 1", path: "/Users/test/Music/tuneKore/Samples/Recordings/Recording 1.wav", seconds, channels: 1, rate: 48000, peaks: peaks(p.index + 2), steps: p.steps });
          publish();
          return { pad: p.index, path: p.path, seconds };
        }
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
        case "sampler_step_extras":
          v.pads[a.pad as number].chance[a.step as number] = a.chance as number;
          v.pads[a.pad as number].ratchet[a.step as number] = a.ratchet as number;
          return publish();
        case "sampler_song":
          Object.assign(v, { chain: a.chain, songMode: a.songMode, songLoop: a.songLoop });
          return publish();
        case "sampler_velocity":
          v.pads[a.pad as number].velocity[a.step as number] = a.velocity as number;
          return publish();
        case "sampler_pattern":
          v.pattern = a.index as number;
          return publish();
        case "sampler_switch":
          v.patternSwitch = a.mode as SamplerView["patternSwitch"];
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
        case "sampler_export_stems":
          return [{ path: `${a.dir}/Beat 1 - 01 Kick.wav`, seconds: 8, format: "pcm24" }];
        case "project_collect":
          return publish();
        case "display_zoom":
          return 1;
        case "autosave_check":
          return null;
        case "opened_files":
          return [];
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
