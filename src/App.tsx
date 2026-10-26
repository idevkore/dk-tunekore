// The main window: the Sampler. Library on the left, pads and the selected
// pad's settings in the middle, the step sequencer below, transport on top.
import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask, open, save } from "@tauri-apps/plugin-dialog";
import logo from "../brand/tunekore-logo-color.svg";
import { onMeter } from "./engine";
import { openAnalyzer } from "./analyzer/api";
import * as api from "./sampler/api";
import { PAD_KEYS, STEP_CHOICES, subscribe, type LibraryItem, type PadSettings, type SamplerView, type Tick } from "./sampler/api";
import Library from "./sampler/Library";
import Pads from "./sampler/Pads";
import Inspector from "./sampler/Inspector";
import Steps from "./sampler/Steps";
import AudioDialog from "./sampler/AudioDialog";
import ExportDialog from "./sampler/ExportDialog";

type Drag =
  | { kind: "sample"; item: LibraryItem; x0: number; y0: number; x: number; y: number; moving: boolean }
  | { kind: "pad"; from: number; name: string; x0: number; y0: number; x: number; y: number; moving: boolean };

const isMac = navigator.platform.toLowerCase().includes("mac");
const mod = isMac ? "⌘" : "Ctrl+";
const DRAG_START_PX = 6;

/** Peak 0..1 to a 0..1 bar position on a -60..0 dB scale. */
function meterPosition(peak: number): number {
  if (peak <= 0) return 0;
  return Math.min(1, Math.max(0, (20 * Math.log10(peak) + 60) / 60));
}

function padAt(x: number, y: number): number | null {
  const el = document.elementFromPoint(x, y)?.closest<HTMLElement>("[data-pad]");
  return el ? Number(el.dataset.pad) : null;
}

const isAudio = (p: string) => api.AUDIO_EXTENSIONS.includes(p.split(".").pop()?.toLowerCase() ?? "");

export default function App() {
  const [view, setView] = useState<SamplerView | null>(null);
  const [tick, setTick] = useState<Tick>({ playing: false, step: null, played: [] });
  const [selected, setSelected] = useState(0);
  const [level, setLevel] = useState(0);
  const [message, setMessage] = useState<{ text: string; error: boolean } | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [dropPad, setDropPad] = useState<number | null>(null);
  const [libraryFolder, setLibraryFolder] = useState<string | null>(null);
  const [libraryRefresh, setLibraryRefresh] = useState(0);
  const [audioOpen, setAudioOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [tempoText, setTempoText] = useState("120");
  const held = useRef(new Set<string>());
  const viewRef = useRef(view);
  viewRef.current = view;
  const libraryRoot = useRef<string | null>(null);

  const say = useCallback((text: string, error = false) => setMessage({ text, error }), []);
  const fail = useCallback((e: unknown) => say(String(e), true), [say]);
  const take = useCallback((v: SamplerView) => setView(v), []);

  // State, ticks and the meter.
  useEffect(() => {
    api.state().then(take).catch(fail);
    const offs = [
      subscribe(api.onState(take)),
      subscribe(api.onTick(setTick)),
      subscribe(onMeter((peak) => setLevel((prev) => Math.max(peak, prev * 0.8)))),
    ];
    return () => offs.forEach((off) => off());
  }, [take, fail]);

  // Rescan the Library when the Analyzer writes samples into it.
  useEffect(
    () =>
      subscribe(
        api.onChopped((folder) => {
          const root = libraryRoot.current;
          if (root && folder.startsWith(root)) setLibraryRefresh((n) => n + 1);
        }),
      ),
    [],
  );

  useEffect(() => {
    if (level === 0) return;
    const t = setTimeout(() => setLevel((l) => (l < 0.0005 ? 0 : l * 0.8)), 60);
    return () => clearTimeout(t);
  }, [level]);

  useEffect(() => {
    if (view) setTempoText(String(Math.round(view.tempo * 10) / 10));
  }, [view?.tempo]);

  // Window title follows the project.
  useEffect(() => {
    if (!view) return;
    const title = `${view.projectName}${view.dirty ? " •" : ""} — tuneKore`;
    document.title = title;
    try {
      getCurrentWindow().setTitle(title).catch(() => {});
    } catch {
      // not in Tauri (the dev mock): the document title is enough
    }
  }, [view?.projectName, view?.dirty]);

  useEffect(() => {
    if (!message || message.error) return;
    const t = setTimeout(() => setMessage(null), 4000);
    return () => clearTimeout(t);
  }, [message]);

  const playing = tick.playing;

  const togglePlay = useCallback(() => {
    api.play(!playing).catch(fail);
  }, [playing, fail]);

  const press = useCallback((pad: number) => void api.trigger(pad).catch(fail), [fail]);
  const unpress = useCallback((pad: number) => void api.release(pad).catch(() => {}), []);

  const loadSample = useCallback(
    (pad: number, path: string) => {
      api
        .loadPad(pad, path)
        .then((v) => {
          take(v);
          setSelected(pad);
        })
        .catch(fail);
    },
    [take, fail],
  );

  // ---- Projects ----

  const keepChanges = useCallback(async () => {
    if (!viewRef.current?.dirty) return true;
    return ask("Your changes to this project aren't saved. Continue without saving?", {
      title: "Unsaved changes",
      kind: "warning",
      okLabel: "Don't save",
      cancelLabel: "Cancel",
    }).catch(() => true);
  }, []);

  const doNew = useCallback(async () => {
    if (!(await keepChanges())) return;
    api.newProject().then((v) => {
      take(v);
      setSelected(0);
    }).catch(fail);
  }, [keepChanges, take, fail]);

  const openPath = useCallback(
    async (path: string) => {
      if (!(await keepChanges())) return;
      api
        .openProject(path)
        .then((v) => {
          take(v);
          setSelected(0);
          const missing = v.pads.filter((p) => p.missing).length;
          say(missing ? `Opened ${v.projectName}. ${missing} sample${missing > 1 ? "s are" : " is"} missing.` : `Opened ${v.projectName}.`, missing > 0);
        })
        .catch(fail);
    },
    [keepChanges, take, fail, say],
  );

  const doOpen = useCallback(async () => {
    const f = await open({ title: "Open a project", filters: [{ name: "tuneKore project", extensions: [api.PROJECT_EXT] }] }).catch(() => null);
    if (typeof f === "string") await openPath(f);
  }, [openPath]);

  const doSave = useCallback(
    async (as: boolean) => {
      let path: string | null = null;
      if (as || !viewRef.current?.projectPath) {
        path = await save({
          title: "Save project",
          defaultPath: `${viewRef.current?.projectName ?? "Untitled"}.${api.PROJECT_EXT}`,
          filters: [{ name: "tuneKore project", extensions: [api.PROJECT_EXT] }],
        }).catch(() => null);
        if (!path) return;
      }
      api.saveProject(path).then((v) => {
        take(v);
        say(`Saved ${v.projectName}.`);
      }).catch(fail);
    },
    [take, fail, say],
  );

  // ---- Keyboard: pads, Space, and the menu shortcuts ----

  useEffect(() => {
    const typing = (t: EventTarget | null) => {
      const el = t as HTMLElement | null;
      if (!el) return false;
      if (el.tagName === "TEXTAREA" || el.tagName === "SELECT") return true;
      return el.tagName === "INPUT" && !["range", "checkbox"].includes((el as HTMLInputElement).type);
    };
    const onDown = (e: KeyboardEvent) => {
      if (document.querySelector("dialog[open]")) return;
      const k = e.key.toLowerCase();
      if (e.metaKey || e.ctrlKey) {
        if (k === "z" && !typing(e.target)) {
          e.preventDefault();
          (e.shiftKey ? api.redo() : api.undo()).then(take).catch(fail);
          return;
        }
        const actions: Record<string, () => void> = {
          s: () => void doSave(e.shiftKey),
          o: () => void doOpen(),
          n: () => void doNew(),
          e: () => setExportOpen(true),
        };
        if (actions[k] && !e.altKey) {
          e.preventDefault();
          actions[k]();
        }
        return;
      }
      if (typing(e.target) || e.altKey) return;
      if (e.key === " ") {
        e.preventDefault();
        if (!e.repeat) togglePlay();
        return;
      }
      const pad = PAD_KEYS.indexOf(k);
      if (pad >= 0) {
        e.preventDefault();
        if (e.repeat || held.current.has(k)) return;
        held.current.add(k);
        setSelected(pad);
        press(pad);
      }
    };
    const onUp = (e: KeyboardEvent) => {
      const k = e.key.toLowerCase();
      if (held.current.delete(k)) unpress(PAD_KEYS.indexOf(k));
    };
    window.addEventListener("keydown", onDown);
    window.addEventListener("keyup", onUp);
    return () => {
      window.removeEventListener("keydown", onDown);
      window.removeEventListener("keyup", onUp);
    };
  }, [togglePlay, press, unpress, doSave, doOpen, doNew, take, fail]);

  // Closing the window or quitting with unsaved changes: the Rust side holds it and asks here.
  useEffect(
    () =>
      subscribe(
        api.onConfirmQuit(() => {
          void ask("Your changes to this project aren't saved. Quit anyway?", {
            title: "Unsaved changes",
            kind: "warning",
            okLabel: "Quit without saving",
            cancelLabel: "Cancel",
          }).then((quit) => { if (quit) void api.quitApp(); });
        }),
      ),
    [],
  );

  // ---- Dragging samples from the Library and pads onto pads ----

  useEffect(() => {
    if (!drag) return;
    const move = (e: PointerEvent) => {
      const moving = drag.moving || Math.hypot(e.clientX - drag.x0, e.clientY - drag.y0) > DRAG_START_PX;
      setDrag({ ...drag, x: e.clientX, y: e.clientY, moving });
      setDropPad(moving ? padAt(e.clientX, e.clientY) : null);
    };
    const up = (e: PointerEvent) => {
      const target = drag.moving ? padAt(e.clientX, e.clientY) : null;
      if (target !== null) {
        if (drag.kind === "sample") loadSample(target, drag.item.path);
        else if (target !== drag.from) {
          api.movePad(drag.from, target).then((v) => {
            take(v);
            setSelected(target);
          }).catch(fail);
        }
      }
      setDrag(null);
      setDropPad(null);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
  }, [drag, loadSample, take, fail]);

  // Files and folders dropped from Finder.
  useEffect(() => {
    let off: (() => void) | undefined;
    let gone = false;
    const dpr = () => window.devicePixelRatio || 1;
    let webview;
    try {
      webview = getCurrentWebview();
    } catch {
      return; // not in Tauri (the dev mock)
    }
    webview
      .onDragDropEvent((e) => {
        const p = e.payload;
        if (p.type === "over") setDropPad(padAt(p.position.x / dpr(), p.position.y / dpr()));
        else if (p.type === "leave") setDropPad(null);
        else if (p.type === "drop") {
          setDropPad(null);
          const project = p.paths.find((f) => f.toLowerCase().endsWith(`.${api.PROJECT_EXT}`));
          if (project) return void openPath(project);
          const files = p.paths.filter(isAudio);
          if (files.length === 0) {
            // A folder: make it the Library.
            if (p.paths.length === 1) setLibraryFolder(p.paths[0]);
            return;
          }
          const v = viewRef.current;
          const at = padAt(p.position.x / dpr(), p.position.y / dpr());
          // Onto a pad and the ones after it; anywhere else, onto the empty pads.
          const targets = at !== null
            ? Array.from({ length: api.PADS - at }, (_, i) => at + i)
            : (v?.pads ?? []).filter((x) => !x.name).map((x) => x.index);
          files.slice(0, targets.length).forEach((f, i) => loadSample(targets[i], f));
          if (files.length > targets.length) say(`${files.length - targets.length} files didn't fit on the pads.`, true);
        }
      })
      .then((u) => (gone ? u() : (off = u)))
      .catch(() => {});
    return () => {
      gone = true;
      off?.();
    };
  }, [loadSample, openPath, say]);

  if (!view) {
    return (
      <div className="boot">
        <img src={logo} alt="tuneKore" className="boot-logo" />
        {message && <p className="msg error">{message.text}</p>}
      </div>
    );
  }

  const pad = view.pads[selected];
  const setTransport = (tempo: number, swing: number, steps: number) => api.setTransport(tempo, swing, steps).then(take).catch(fail);
  const commitTempo = () => {
    const t = Number(tempoText);
    if (Number.isFinite(t) && t >= 40 && t <= 300) void setTransport(t, view.swing, view.steps);
    else setTempoText(String(view.tempo));
  };
  const nudgeTempo = (d: number) => void setTransport(Math.min(300, Math.max(40, Math.round(view.tempo + d))), view.swing, view.steps);
  const hasPattern = view.pads.some((p) => p.name && p.steps.some(Boolean));

  return (
    <div className={`app${drag?.moving ? " dragging" : ""}`}>
      <header className="bar">
        <img src={logo} alt="tuneKore" className="bar-logo" />
        <div className="project" title={view.projectPath ?? "Not saved yet"}>
          <span className="project-name">{view.projectName}</span>
          {view.dirty && <span className="dirty" aria-label="unsaved changes">•</span>}
        </div>
        <div className="group-btns">
          <button className="btn small" onClick={() => void doNew()} title={`New project (${mod}N)`}>New</button>
          <button className="btn small" onClick={() => void doOpen()} title={`Open a project (${mod}O)`}>Open</button>
          <button className="btn small" onClick={() => void doSave(false)} title={`Save (${mod}S); Save As: ${mod}⇧S`}>Save</button>
          <button className="btn small icon" onClick={() => void api.undo().then(take).catch(fail)} disabled={!view.canUndo} aria-label="Undo" title={`Undo (${mod}Z)`}>
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M5.5 4 2.5 7l3 3M3 7h6.5a3.5 3.5 0 0 1 0 7H7" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" /></svg>
          </button>
          <button className="btn small icon" onClick={() => void api.redo().then(take).catch(fail)} disabled={!view.canRedo} aria-label="Redo" title={`Redo (${mod}⇧Z)`}>
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M10.5 4l3 3-3 3M13 7H6.5a3.5 3.5 0 0 0 0 7H9" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" /></svg>
          </button>
        </div>

        <div className="transport" role="group" aria-label="Transport">
          <button className={`play${playing ? " on" : ""}`} onClick={togglePlay} aria-label={playing ? "Stop" : "Play"} title="Play or stop (Space)">
            {playing ? (
              <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><rect x="3" y="3" width="10" height="10" rx="1.5" fill="currentColor" /></svg>
            ) : (
              <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M4 2.5v11l9.5-5.5z" fill="currentColor" /></svg>
            )}
          </button>
          <div className="tempo">
            <button className="nudge" onClick={() => nudgeTempo(-1)} aria-label="Slower">−</button>
            <input
              aria-label="Tempo in BPM"
              className="mono"
              inputMode="decimal"
              value={tempoText}
              onChange={(e) => setTempoText(e.target.value)}
              onBlur={commitTempo}
              onKeyDown={(e) => {
                if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                if (e.key === "ArrowUp" || e.key === "ArrowDown") {
                  e.preventDefault();
                  nudgeTempo(e.key === "ArrowUp" ? 1 : -1);
                }
              }}
            />
            <span className="unit">BPM</span>
            <button className="nudge" onClick={() => nudgeTempo(1)} aria-label="Faster">+</button>
          </div>
          <label className="swing" title="Swing: delays every other sixteenth">
            <span>Swing</span>
            <input type="range" min={0} max={1} step={0.01} value={view.swing} onChange={(e) => void setTransport(view.tempo, Number(e.target.value), view.steps)} />
            <span className="mono">{Math.round(view.swing * 100)}%</span>
          </label>
          <label className="len">
            <span>Steps</span>
            <select value={view.steps} onChange={(e) => void setTransport(view.tempo, view.swing, Number(e.target.value))}>
              {STEP_CHOICES.map((n) => <option key={n} value={n}>{n}</option>)}
            </select>
          </label>
        </div>

        <div className="grow" />
        <button className="btn small" onClick={() => setExportOpen(true)} disabled={!hasPattern} title={`Export the pattern as WAV, or bounce it onto a pad (${mod}E)`}>Export</button>
        <button className="btn small" onClick={() => openAnalyzer().catch(fail)} title="Split a song into stems, then send them to pads">Analyzer</button>
        <button className="btn small icon" onClick={() => setAudioOpen(true)} aria-label="Audio settings" title="Audio settings">
          <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true"><path d="M2 6h3l4-3v10L5 10H2zM11.5 5.5a3.5 3.5 0 0 1 0 5M13 3.5a6 6 0 0 1 0 9" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" /></svg>
        </button>
        <div className="meter" role="meter" aria-label="Output level" aria-valuemin={0} aria-valuemax={1} aria-valuenow={meterPosition(level)}>
          <div className="meter-fill" style={{ transform: `scaleX(${meterPosition(level)})` }} />
        </div>
      </header>

      <main className="body">
        <Library
          selectedPad={selected}
          onLoad={(item) => loadSample(selected, item.path)}
          onDragStart={(item, x, y) => setDrag({ kind: "sample", item, x0: x, y0: y, x, y, moving: false })}
          onError={fail}
          refresh={libraryRefresh}
          folder={libraryFolder}
          onFolder={(d) => (libraryRoot.current = d)}
        />
        <div className="center">
          <div className="top-row">
            <Pads
              pads={view.pads}
              selected={selected}
              played={tick.played}
              dropTarget={dropPad}
              onSelect={setSelected}
              onPress={press}
              onRelease={unpress}
              onDragStart={(from, x, y) => setDrag({ kind: "pad", from, name: view.pads[from].name ?? "", x0: x, y0: y, x, y, moving: false })}
            />
            <Inspector
              pad={pad}
              onSettings={(s: PadSettings) => void api.setSettings(selected, s).then(take).catch(fail)}
              onLoadFile={(f) => loadSample(selected, f)}
              onClear={() => void api.clearPad(selected).then(take).catch(fail)}
              onClearSteps={() => void api.clearSteps(selected).then(take).catch(fail)}
              onAudition={() => {
                press(selected);
                setTimeout(() => unpress(selected), 300);
              }}
            />
          </div>
          <Steps
            pads={view.pads}
            steps={view.steps}
            current={tick.step}
            selected={selected}
            onSelect={setSelected}
            onPress={press}
            onRelease={unpress}
            onStep={(p, s, on) => void api.setStep(p, s, on).then(take).catch(fail)}
            onMute={(p) => void api.setSettings(p, { ...view.pads[p].settings, mute: !view.pads[p].settings.mute }).then(take).catch(fail)}
            dropTarget={dropPad}
            onAddFiles={(files) => {
              const free = view.pads.filter((x) => !x.name).map((x) => x.index);
              files.slice(0, free.length).forEach((f, i) => loadSample(free[i], f));
              if (files.length > free.length) say(`${files.length - free.length} files didn't fit on the pads.`, true);
            }}
          />
        </div>
      </main>

      <footer className="status" aria-live="polite">
        {message ? (
          <span className={`msg${message.error ? " error" : ""}`}>
            {message.text}
            {message.error && <button className="link" onClick={() => setMessage(null)}>Dismiss</button>}
          </span>
        ) : (
          <span className="hints">
            <kbd>Space</kbd> play · <kbd>1</kbd>–<kbd>V</kbd> pads · <kbd>↑</kbd><kbd>↓</kbd> browse · <kbd>Enter</kbd> load on pad {selected + 1} · <kbd>/</kbd> search
          </span>
        )}
      </footer>

      {drag?.moving && (
        <div className="ghost" style={{ left: drag.x + 12, top: drag.y + 8 }} aria-hidden="true">
          {drag.kind === "sample" ? drag.item.name : `Pad ${drag.from + 1}: ${drag.name}`}
        </div>
      )}

      {audioOpen && <AudioDialog onClose={() => setAudioOpen(false)} onError={fail} />}
      {exportOpen && (
        <ExportDialog
          name={view.projectName}
          steps={view.steps}
          tempo={view.tempo}
          onClose={() => setExportOpen(false)}
          onDone={(p) => say(`Exported ${p}.`)}
          onBounce={(loops) =>
            api
              .bounce(loops)
              .then((b) => {
                setSelected(b.pad);
                say(`Bounced the pattern onto pad ${b.pad + 1}.`);
              })
              .catch(fail)
          }
          onError={fail}
          disabled={!hasPattern}
        />
      )}
    </div>
  );
}
