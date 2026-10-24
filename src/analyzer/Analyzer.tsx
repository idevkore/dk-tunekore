import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import logo from "../../brand/musikore-logo-color.svg";
import Waveform from "./Waveform";
import * as api from "./api";
import type { Backend, Finished, SongInfo } from "./api";

const STEMS = [
  { name: "vocals", label: "Vocals", color: "#f2c14e" },
  { name: "drums", label: "Drums", color: "#ff8a5b" },
  { name: "bass", label: "Bass", color: "#4fc3f7" },
  { name: "other", label: "Other", color: "#d08bff" },
];

const STAGE_LABELS: Record<string, string> = {
  load: "Load the model",
  separate: "Separate",
  vocals: "Vocals",
  drums: "Drums",
  bass: "Bass",
  other: "Other",
  write: "Write the stems",
};

const MODES = [
  { id: "fast", label: "Fast", text: "One pass for all four stems. Good for sketching." },
  { id: "best", label: "Best", text: "More passes for cleaner stems. Takes longer." },
];

const MIN_DB = -40;

type Phase =
  | { kind: "empty" }
  | { kind: "opening" }
  | { kind: "ready" }
  | { kind: "separating"; job: number; started: number; stages: string[]; fraction: number }
  | { kind: "stems"; result: Finished };

interface Mix {
  db: number;
  mute: boolean;
  solo: boolean;
}

const freshMix = (): Mix[] => STEMS.map(() => ({ db: 0, mute: false, solo: false }));
const gainOf = (db: number) => (db <= MIN_DB ? 0 : Math.pow(10, db / 20));

function clock(s: number): string {
  if (!isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  return `${m}:${sec.toString().padStart(2, "0")}`;
}

/** A duration in words: "20 s", "1 min 41 s". */
function took(s: number): string {
  const r = Math.round(s);
  return r < 60 ? `${r} s` : `${Math.floor(r / 60)} min${r % 60 ? ` ${r % 60} s` : ""}`;
}

function songMeta(s: SongInfo): string {
  const khz = (s.sampleRate / 1000).toFixed(s.sampleRate % 1000 === 0 ? 0 : 1);
  const bits = s.bits ? ` · ${s.bits}-bit` : "";
  return `${clock(s.seconds)} · ${khz} kHz${bits} · ${s.codec} · ${s.channels === 1 ? "mono" : "stereo"}`;
}

export default function Analyzer() {
  const [song, setSong] = useState<SongInfo | null>(null);
  const [backends, setBackends] = useState<Backend[]>([]);
  const [backend, setBackend] = useState("demucs");
  const [mode, setMode] = useState("fast");
  const [phase, setPhase] = useState<Phase>({ kind: "empty" });
  const [error, setError] = useState("");
  const [note, setNote] = useState("");
  const [dragging, setDragging] = useState(false);
  const [pos, setPos] = useState({ position: 0, duration: 0, playing: false });
  const [mix, setMix] = useState<Mix[]>(freshMix);
  const [listen, setListen] = useState<"stems" | "original">("stems");
  const [lastDir, setLastDir] = useState<string | null>(null);
  const jobRef = useRef<number | null>(null);

  const loadBackends = useCallback(() => {
    api
      .backends()
      .then((b) => {
        setBackends(b);
        setBackend((cur) => (b.find((x) => x.id === cur)?.available ? cur : (b.find((x) => x.available)?.id ?? cur)));
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(loadBackends, [loadBackends]);

  // Worker events. A job id that isn't the current one is stale.
  useEffect(() => {
    const offs: Promise<() => void>[] = [
      api.onProgress((p) => {
        if (p.job !== jobRef.current) return;
        setPhase((ph) => {
          if (ph.kind !== "separating") return ph;
          const stages = ph.stages.includes(p.stage) ? ph.stages : [...ph.stages, p.stage];
          return { ...ph, stages, fraction: Math.max(ph.fraction, p.fraction) };
        });
      }),
      api.onDone((r) => {
        if (r.job !== jobRef.current) return;
        jobRef.current = null;
        setMix(freshMix());
        setListen("stems");
        setPhase({ kind: "stems", result: r });
      }),
      api.onFailed((f) => {
        if (f.job !== jobRef.current) return;
        jobRef.current = null;
        setPhase({ kind: "ready" });
        if (!f.cancelled) setError(f.message);
      }),
      api.onPosition((p) => setPos(p)),
    ];
    return () => offs.forEach((o) => o.then((u) => u()).catch(() => {}));
  }, []);

  const openPath = useCallback(async (path: string) => {
    setError("");
    setNote("");
    jobRef.current = null;
    setPhase({ kind: "opening" });
    try {
      await api.play(false).catch(() => {});
      const s = await api.openSong(path);
      setSong(s);
      setPos({ position: 0, duration: s.seconds, playing: false });
      setPhase({ kind: "ready" });
    } catch (e) {
      setPhase(song ? { kind: "ready" } : { kind: "empty" });
      setError(String(e));
    }
  }, [song]);

  async function chooseSong() {
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Audio", extensions: api.SONG_EXTENSIONS }],
    });
    if (typeof picked === "string") void openPath(picked);
  }

  // Drop a song anywhere on the window.
  useEffect(() => {
    const off = getCurrentWebview().onDragDropEvent((e) => {
      const t = e.payload.type;
      if (t === "enter" || t === "over") setDragging(true);
      else if (t === "leave") setDragging(false);
      else if (t === "drop") {
        setDragging(false);
        const path = e.payload.paths.find((p) => api.SONG_EXTENSIONS.includes(p.split(".").pop()?.toLowerCase() ?? ""));
        if (path) void openPath(path);
        else setError("Drop a WAV, AIFF, FLAC, MP3 or M4A file.");
      }
    });
    return () => void off.then((u) => u());
  }, [openPath]);

  // Keep the player's tracks in step with the mixer. Track 0 is the original.
  const hasStems = phase.kind === "stems";
  useEffect(() => {
    if (!hasStems) {
      void api.setTrack(0, 1, false, false);
      return;
    }
    const original = listen === "original";
    void api.setTrack(0, 1, !original, original);
    mix.forEach((m, i) => void api.setTrack(i + 1, gainOf(m.db), m.mute, m.solo && !original));
  }, [hasStems, mix, listen]);

  async function startSeparation() {
    setError("");
    setNote("");
    try {
      await api.play(false);
      const job = await api.separate(backend, mode);
      jobRef.current = job;
      setPhase({ kind: "separating", job, started: Date.now(), stages: [], fraction: 0 });
    } catch (e) {
      setError(String(e));
    }
  }

  async function togglePlay() {
    try {
      await api.play(!pos.playing);
      setPos((p) => ({ ...p, playing: !p.playing }));
    } catch (e) {
      setError(String(e));
    }
  }

  // Space plays and pauses, unless typing in a field.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.code !== "Space" || !song || phase.kind === "separating") return;
      const tag = (e.target as HTMLElement).tagName;
      if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA" || tag === "BUTTON") return;
      e.preventDefault();
      void togglePlay();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  function seekTo(fraction: number) {
    const s = fraction * (pos.duration || song?.seconds || 0);
    setPos((p) => ({ ...p, position: s }));
    void api.seek(s);
  }

  async function exportTo(names: string[]) {
    setError("");
    const dir = await open({ directory: true, multiple: false, defaultPath: lastDir ?? undefined, title: "Export stems to" });
    if (typeof dir !== "string") return;
    setLastDir(dir);
    try {
      const out = await api.exportStems(dir, names);
      const float = out.some((o) => o.format === "float32");
      setNote(
        `Saved ${out.length === 1 ? out[0].path.split("/").pop() : `${out.length} WAV files`} to ${dir}` +
          (float ? ". Some stems go above full scale, so they were saved as 32-bit float." : "."),
      );
    } catch (e) {
      setError(String(e));
    }
  }

  const update = (i: number, change: Partial<Mix>) => setMix((m) => m.map((x, j) => (j === i ? { ...x, ...change } : x)));
  const anySolo = mix.some((m) => m.solo);
  const playhead = pos.duration > 0 ? Math.min(1, pos.position / pos.duration) : 0;
  const chosen = backends.find((b) => b.id === backend);
  const busy = phase.kind === "separating" || phase.kind === "opening";

  return (
    <div className={"analyzer" + (dragging ? " dragging" : "")}>
      <header className="bar">
        <img src={logo} alt="musiKore" className="bar-logo" />
        <h1>Analyzer</h1>
        <span className="sub">Split a song into stems</span>
      </header>

      <div className="body">
        <aside className="side">
          <section className="group">
            <h2>Song</h2>
            {song ? (
              <div className="song-card">
                <div className="song-head">
                  <span className="song-icon" aria-hidden="true">
                    <svg width="22" height="22" viewBox="0 0 16 16">
                      <path d="M2 8h1M4.5 5.5v5M7 3v10M9.5 5v6M12 6.5v3M14.5 8h-.5" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
                    </svg>
                  </span>
                  <span className="song-text">
                    <span className="song-name" title={song.path}>{song.name}</span>
                    <span className="mono dim">{songMeta(song)}</span>
                  </span>
                  <button className="btn small" onClick={() => void chooseSong()} disabled={busy}>
                    Replace
                  </button>
                </div>
                <Waveform peaks={song.peaks} color="#8a8a98" playhead={null} height={44} label={`${song.name} waveform`} />
              </div>
            ) : (
              <button className="drop" onClick={() => void chooseSong()} disabled={busy}>
                <strong>{phase.kind === "opening" ? "Opening…" : "Open a song"}</strong>
                <span>or drop one on this window. WAV, AIFF, FLAC, MP3 or M4A.</span>
              </button>
            )}
          </section>

          <fieldset className="group" disabled={busy}>
            <legend>Model</legend>
            {backends.length === 0 && !error && <span className="card-note">Checking which models this Mac has…</span>}
            {backends.map((b) => (
              <label key={b.id} className={"card" + (backend === b.id ? " on" : "") + (b.available ? "" : " off")}>
                <input type="radio" name="backend" checked={backend === b.id} disabled={!b.available} onChange={() => setBackend(b.id)} />
                <span className="card-text">
                  <span className="card-title">{b.name}</span>
                  <span className="card-note">
                    {b.available ? (b.id === "demucs" ? "Research-only weights: for testing on this Mac." : "Meta's model, under the SAM License.") : b.reason}
                  </span>
                </span>
              </label>
            ))}
            {backends.length > 0 && backends.some((b) => !b.available) && (
              <button className="link" type="button" onClick={loadBackends}>
                Check again
              </button>
            )}
          </fieldset>

          <fieldset className="group" disabled={busy}>
            <legend>Mode</legend>
            {MODES.map((m) => (
              <label key={m.id} className={"card" + (mode === m.id ? " on" : "")}>
                <input type="radio" name="mode" checked={mode === m.id} onChange={() => setMode(m.id)} />
                <span className="card-text">
                  <span className="card-title">{m.label}</span>
                  <span className="card-note">{m.text}</span>
                  {chosen?.modeLabels[m.id] && <span className="mono dim">{chosen.modeLabels[m.id]}</span>}
                </span>
              </label>
            ))}
          </fieldset>

          {song && phase.kind !== "separating" && (
            <button className="btn primary" onClick={() => void startSeparation()} disabled={busy || !chosen?.available}>
              {phase.kind === "stems" ? "Separate again" : "Separate"}
            </button>
          )}

          <span className="grow" />
          <p className="privacy">
            <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
              <rect x="3" y="7" width="10" height="7" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.4" />
              <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" fill="none" stroke="currentColor" strokeWidth="1.4" />
            </svg>
            Runs on this Mac. Nothing is uploaded.
          </p>
        </aside>

        <main className="main">
          {error && (
            <div className="error" role="alert">
              <pre>{error}</pre>
              <button className="link" onClick={() => setError("")}>
                Dismiss
              </button>
            </div>
          )}

          {phase.kind === "empty" || phase.kind === "opening" ? (
            <div className="empty">
              <h2>Open a song to split it into vocals, drums, bass and other.</h2>
              <p className="dim">Separation runs in its own process on this Mac, so the app stays responsive.</p>
            </div>
          ) : phase.kind === "separating" ? (
            <Separating phase={phase} song={song!} mode={mode} onCancel={() => void api.cancel()} />
          ) : (
            <>
              <div className="head">
                <div className="head-text">
                  <h2>
                    {song!.name}
                    {phase.kind === "stems" ? " · 4 stems" : ""}
                  </h2>
                  <span className="dim">
                    {phase.kind === "stems" ? <Summary result={phase.result} backends={backends} /> : "Not separated yet. Play it, or press Separate."}
                  </span>
                </div>
                <div className="transport" role="group" aria-label="Player">
                  <button className={"play" + (pos.playing ? " on" : "")} aria-label={pos.playing ? "Pause" : "Play"} aria-pressed={pos.playing} onClick={() => void togglePlay()}>
                    {pos.playing ? (
                      <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
                        <path d="M4.5 3h2.5v10H4.5zM9 3h2.5v10H9z" fill="currentColor" />
                      </svg>
                    ) : (
                      <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
                        <path d="M5 3.5v9l7-4.5z" fill="currentColor" />
                      </svg>
                    )}
                  </button>
                  <span className="mono time">
                    {clock(pos.position)} <span className="dim">/ {clock(pos.duration || song!.seconds)}</span>
                  </span>
                  {phase.kind === "stems" && (
                    <span className="ab" role="radiogroup" aria-label="Listen to">
                      <button role="radio" aria-checked={listen === "stems"} className={listen === "stems" ? "on" : ""} onClick={() => setListen("stems")}>
                        Stems
                      </button>
                      <button role="radio" aria-checked={listen === "original"} className={listen === "original" ? "on" : ""} onClick={() => setListen("original")}>
                        Original
                      </button>
                    </span>
                  )}
                </div>
              </div>

              {phase.kind === "ready" && (
                <div className="row">
                  <span className="chip" style={{ background: "#8a8a98" }} />
                  <span className="row-name">Song</span>
                  <Waveform peaks={song!.peaks} color="#c9c9d2" playhead={playhead} playing={pos.playing} label="Song position" onSeek={seekTo} />
                </div>
              )}

              {phase.kind === "stems" && (
                <section className="stems" aria-label="Stems">
                  {STEMS.map((s, i) => {
                    const info = phase.result.stems.find((x) => x.name === s.name);
                    const m = mix[i];
                    const audible = listen === "stems" && !m.mute && (!anySolo || m.solo) && m.db > MIN_DB;
                    return (
                      <div className="row" key={s.name}>
                        <span className="chip" style={{ background: s.color }} />
                        <span className="row-name">{s.label}</span>
                        <Waveform
                          peaks={info?.peaks ?? []}
                          color={s.color}
                          dim={!audible}
                          playhead={playhead}
                          playing={pos.playing}
                          label={`${s.label} position`}
                          onSeek={seekTo}
                        />
                        <button className={"sq" + (m.solo ? " solo" : "")} aria-label={`Solo ${s.label}`} aria-pressed={m.solo} onClick={() => update(i, { solo: !m.solo })}>
                          S
                        </button>
                        <button className={"sq" + (m.mute ? " mute" : "")} aria-label={`Mute ${s.label}`} aria-pressed={m.mute} onClick={() => update(i, { mute: !m.mute })}>
                          M
                        </button>
                        <label className="level">
                          <span>
                            Level <span className="mono">{m.db <= MIN_DB ? "-inf" : `${m.db > 0 ? "+" : ""}${m.db.toFixed(1)} dB`}</span>
                          </span>
                          <input
                            type="range"
                            min={MIN_DB}
                            max={6}
                            step={0.5}
                            value={m.db}
                            onChange={(e) => update(i, { db: Number(e.target.value) })}
                            onDoubleClick={() => update(i, { db: 0 })}
                            aria-label={`${s.label} level`}
                          />
                        </label>
                        <button className="btn small" onClick={() => void exportTo([s.name])} aria-label={`Export ${s.label} as WAV`}>
                          <svg width="13" height="13" viewBox="0 0 16 16" aria-hidden="true">
                            <path d="M8 2.5v8M4.5 7 8 10.5 11.5 7M3 13.5h10" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
                          </svg>
                          WAV
                        </button>
                      </div>
                    );
                  })}
                </section>
              )}

              {phase.kind === "stems" && (
                <div className="later">
                  <span className="label">Next with one stem · coming later</span>
                  <div className="chips">
                    {["Find key and tempo", "Turn into MIDI", "Chop into a sampler", "Send through a plugin chain"].map((t) => (
                      <button key={t} disabled>
                        {t}
                      </button>
                    ))}
                  </div>
                </div>
              )}

              <span className="grow" />
              {note && <p className="note">{note}</p>}
              {phase.kind === "stems" && (
                <footer className="foot">
                  <span className="dim">Four WAV files, 24-bit, the same length and rate as the song.</span>
                  <span className="grow" />
                  <button className="btn" onClick={() => void exportTo([])}>
                    Export all
                  </button>
                </footer>
              )}
            </>
          )}
        </main>
      </div>
      {dragging && <div className="dropzone">Drop the song to open it</div>}
    </div>
  );
}

function Summary({ result, backends }: { result: Finished; backends: Backend[] }) {
  const name = backends.find((b) => b.id === result.backend)?.name ?? result.backend;
  const c = result.checks;
  const aligned = c.sameLength && c.lagFrames === 0;
  const snr = isFinite(c.sumSnrDb) ? `${c.sumSnrDb.toFixed(1)} dB` : "exactly";
  return (
    <>
      {name}, {result.mode === "best" ? "Best" : "Fast"} mode, in {took(result.seconds)}.{" "}
      <span className={aligned ? "ok" : "warn"}>
        {aligned ? "Lines up with the song to the sample" : `Off by ${c.lagFrames} samples`}; sums back to it at {snr} SNR.
      </span>
    </>
  );
}

function Separating({
  phase,
  song,
  mode,
  onCancel,
}: {
  phase: Extract<Phase, { kind: "separating" }>;
  song: SongInfo;
  mode: string;
  onCancel: () => void;
}) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, []);
  const elapsed = (now - phase.started) / 1000;
  const f = phase.fraction;
  const left = f > 0.12 ? (elapsed * (1 - f)) / f : null;
  const pct = Math.round(f * 100);
  const steps = ["decode", ...phase.stages.filter((s) => s !== "decode"), ...(phase.stages.includes("write") ? [] : ["write"])];
  const current = phase.stages[phase.stages.length - 1];
  return (
    <div className="separating">
      <div className="head">
        <div className="head-text">
          <h2>Separating {song.name}</h2>
          <span className="dim">
            {mode === "best" ? "Best" : "Fast"} mode · {took(elapsed)} so far
            {left !== null ? ` · about ${left < 60 ? `${Math.max(5, Math.round(left / 5) * 5)} s` : `${Math.round(left / 60)} min`} left` : ""}
          </span>
        </div>
        <button className="btn" onClick={onCancel}>
          Cancel
        </button>
      </div>
      <div className="progress">
        <div role="progressbar" aria-label="Separation progress" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct}>
          <div style={{ transform: `scaleX(${f})` }} />
        </div>
        <span className="mono">{pct}%</span>
      </div>
      <ol className="steps">
        {steps.map((s) => {
          const state = s === "decode" ? "done" : s === current ? "active" : phase.stages.includes(s) ? "done" : "waiting";
          return (
            <li key={s} className={state}>
              {STAGE_LABELS[s] ?? (s === "decode" ? "Decode" : s)}
            </li>
          );
        })}
      </ol>
      <section className="stems" aria-label="Stems so far">
        {STEMS.map((s) => (
          <div className="row ghost" key={s.name}>
            <span className="chip" style={{ background: s.color }} />
            <span className="row-name">{s.label}</span>
            <Waveform peaks={song.peaks} color={s.color} dim playhead={null} reveal={f} label={`${s.label}, in progress`} />
          </div>
        ))}
      </section>
      <p className="dim small">
        Separation runs in its own process. Cancel stops it at once; the song stays open.
      </p>
    </div>
  );
}
