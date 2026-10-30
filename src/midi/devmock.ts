// Dev only: MIDI learn for the Sampler's browser mock (src/sampler/devmock.ts).
// `window.__mockMidi(channel, cc, value)` stands in for a controller's knob:
// while learning it maps, otherwise it moves what it's mapped to (jumping,
// on a plain linear or log scale; the real curves are in Rust).
import { emit } from "@tauri-apps/api/event";
import type { PadSettings, SamplerView } from "../sampler/api";
import { MASTER, PAD_CONTROLS, SWING, TEMPO, type MappingView, type MidiView, type TargetInfo } from "./api";

const RANGES: Record<string, [number, number]> = {
  level: [-60, 6], pan: [-1, 1], pitch: [-24, 24], start: [0, 1], end: [0, 1], cutoff: [20, 20000], resonance: [0, 1],
  attack: [0, 2], decay: [0, 5], sustain: [0, 1], release: [0.001, 5], fenvAmount: [-8, 8], fenvAttack: [0, 2], fenvDecay: [0, 5],
  fenvSustain: [0, 1], fenvRelease: [0.001, 5], velCutoff: [-8, 8], velTime: [-1, 1],
};

export function installMidiMock(v: SamplerView, publish: () => unknown) {
  const view: MidiView = { mappings: [], learning: null, takeover: "pickup" };
  let focus = 0;
  // Like the app: small updates while a knob turns, the full view once it stops.
  let settle: ReturnType<typeof setTimeout> | undefined;
  const pads: [string, string][] = Object.entries(PAD_CONTROLS).map(([, c]) => [c!.name, c!.label]);
  const targets = (): TargetInfo[] => [
    { id: TEMPO, label: "Tempo", group: "Transport" },
    { id: SWING, label: "Swing", group: "Transport" },
    { id: MASTER, label: "Master level", group: "Output" },
    { id: "track.1.volume", label: "Volume", group: "Track 1 · Sampler" },
    { id: "track.1.pan", label: "Pan", group: "Track 1 · Sampler" },
    ...["selected", ...Array.from({ length: 16 }, (_, i) => String(i + 1))].flatMap((p) =>
      pads.map(([name, label]) => ({ id: `sampler.pad.${p}.${name}`, label, group: p === "selected" ? "Selected pad" : `Pad ${p}` })),
    ),
  ];
  const tell = () => {
    void emit("midi:mappings", structuredClone(view));
    return structuredClone(view);
  };

  (window as unknown as { __mockMidi: (channel: number, cc: number, value: number) => void }).__mockMidi = (channel, cc, value) => {
    if (view.learning) {
      const t = targets().find((x) => x.id === view.learning)!;
      const m: MappingView = { target: t.id, label: t.label, group: t.group, source: { channel, cc, bits: 7 }, controller: `Ch ${channel} · CC ${cc}`, available: true };
      view.mappings = view.mappings.filter((x) => x.target !== t.id && !(x.source.channel === channel && x.source.cc === cc));
      view.mappings.push(m);
      view.learning = null;
      v.dirty = true;
      void emit("midi:learned", m);
      tell();
      publish();
      return;
    }
    const m = view.mappings.find((x) => x.source.channel === channel && x.source.cc === cc);
    if (!m) return;
    const t = value / 127;
    let moved = 0;
    const pad = /^sampler\.pad\.(selected|\d+)\.(\w+)$/.exec(m.target);
    if (pad) {
      const [lo, hi] = RANGES[pad[2]];
      moved = pad[2] === "cutoff" ? lo * Math.pow(hi / lo, t) : lo + (hi - lo) * t;
      if (pad[2] === "pitch") moved = Math.round(moved);
      const key = Object.entries(PAD_CONTROLS).find(([, c]) => c!.name === pad[2])![0] as keyof PadSettings;
      const settings = v.pads[pad[1] === "selected" ? focus : Number(pad[1]) - 1].settings as unknown as Record<string, number>;
      settings[key] = moved;
    } else if (m.target === TEMPO) {
      v.tempo = moved = Math.round(40 + 260 * t);
    } else if (m.target === SWING) {
      v.swing = moved = Math.round(t * 100) / 100;
    } else if (m.target === MASTER) {
      moved = t === 0 ? -60 : Math.max(-60, 20 * Math.log10(2 * t * t * t));
    }
    v.dirty = true;
    void emit("sampler:controls", { pads: v.pads.map((p) => structuredClone(p.settings)), tempo: v.tempo, swing: v.swing });
    void emit("midi:moved", [{ target: m.target, value: moved }]);
    clearTimeout(settle);
    settle = setTimeout(publish, 500);
  };

  return (cmd: string, a: Record<string, unknown>): unknown => {
    switch (cmd) {
      case "midi_mappings":
        return structuredClone(view);
      case "midi_targets":
        return targets();
      case "midi_learn":
        view.learning = (a.target as string | null) ?? null;
        return tell();
      case "midi_forget":
        view.mappings = view.mappings.filter((x) => x.target !== a.target);
        v.dirty = true;
        publish();
        return tell();
      case "midi_focus":
        focus = a.pad as number;
        return null;
      case "midi_takeover":
        view.takeover = a.mode as MidiView["takeover"];
        return tell();
      default:
        return undefined;
    }
  };
}
