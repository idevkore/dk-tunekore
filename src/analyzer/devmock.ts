// Dev only: stands in for the Rust side so the Analyzer window can be tried
// in a plain browser at http://localhost:1420/analyzer.html?mock.
// main.tsx loads it only in `npm run dev` builds with `?mock` in the URL.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";

const peaks = (seed: number) =>
  Array.from({ length: 1200 }, (_, i) => Math.min(1, Math.abs(Math.sin(i * 0.05 * seed) * 0.6 + Math.sin(i * 0.31) * 0.3)));

export function installMocks() {
  mockWindows("analyzer");
  let playing = false;
  let position = 0;
  const calls: unknown[] = [];
  (window as unknown as { __mockCalls: unknown[] }).__mockCalls = calls;
  setInterval(() => {
    if (!playing) return;
    position += 0.1;
    void emit("analyzer:position", { position, duration: 193.2, playing: true });
  }, 100);

  mockIPC(
    (cmd, args) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case "analyzer_backends":
          return [
            { id: "demucs", name: "Demucs (htdemucs)", modes: ["fast", "best"], modeLabels: { fast: "htdemucs, one pass", best: "htdemucs_ft, one model per stem" }, available: true, reason: null },
          ];
        case "plugin:dialog|open":
          return (args as { options?: { directory?: boolean } })?.options?.directory ? "/Users/test/Stems" : "/Users/test/Music/Quantum Love.mp3";
        case "analyzer_open":
          return { name: "Quantum Love", path: "/Users/test/Music/Quantum Love.mp3", seconds: 193.2, sampleRate: 48000, channels: 2, codec: "MP3", bits: null, peaks: peaks(1) };
        case "analyzer_separate": {
          const stages = ["load", "separate", "write"];
          let i = 0;
          const t = setInterval(() => {
            i++;
            void emit("analyzer:progress", { job: 1, stage: stages[Math.min(2, Math.floor(i / 8))], fraction: Math.min(0.99, i / 25) });
            if (i >= 25) {
              clearInterval(t);
              void emit("analyzer:done", {
                job: 1, backend: "demucs", mode: "fast", seconds: 20.5, peakRssMb: 1792, peakGpuMb: 1225,
                checks: { sameLength: true, sumSnrDb: 29.8, lagFrames: 0 },
                stems: ["vocals", "drums", "bass", "other"].map((name, k) => ({ name, peaks: peaks(k + 2), rmsDb: -20 - k })),
              });
            }
          }, 80);
          return 1;
        }
        case "analyzer_play":
          playing = (args as { playing: boolean }).playing;
          if (!playing) void emit("analyzer:position", { position, duration: 193.2, playing: false });
          return null;
        case "analyzer_seek":
          position = (args as { seconds: number }).seconds;
          return null;
        case "analyzer_export":
          return ["vocals", "drums", "bass", "other"]
            .filter((n) => (args as { names: string[] }).names.length === 0 || (args as { names: string[] }).names.includes(n))
            .map((n) => ({ path: `/Users/test/Stems/Quantum Love - ${n}.wav`, format: "pcm24" }));
        default:
          return null;
      }
    },
    { shouldMockEvents: true },
  );
}
