// Export the project: the whole song or the loop range, every track through
// its devices and plugins, then the master level and limiter, as a master WAV
// and stems (one per heard track), at the project's rate. Opened from File >
// Export Project… (⇧⌘R). The Sampler's own export (⌘E) stays as it is.
//
// The render runs in the engine, faster than real time; this shows how far it
// has got and can cancel it, which leaves nothing half written.
import { useEffect, useRef, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import Dialog from "../sampler/Dialog";
import { subscribe } from "../timeline/api";
import type { Message } from "../StatusBar";
import * as api from "./api";
import { bars, duration, type ExportInfo, type ExportProgress, type Format, type Range } from "./api";
import "./export.css";

const FORMATS: { id: Format; label: string }[] = [
  { id: "pcm16", label: "16-bit" },
  { id: "pcm24", label: "24-bit" },
  { id: "float32", label: "32-bit float" },
];

interface Props {
  onClose: () => void;
  onMessage: (m: Message | null) => void;
}

const fileName = (path: string) => path.split(/[\\/]/).pop() ?? path;

export default function ProjectExport({ onClose, onMessage }: Props) {
  const [info, setInfo] = useState<ExportInfo | null>(null);
  const [range, setRange] = useState<Range>("song");
  const [master, setMaster] = useState(true);
  const [stems, setStems] = useState(false);
  const [format, setFormat] = useState<Format>("pcm24");
  const [tail, setTail] = useState(true);
  const [running, setRunning] = useState<ExportProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The latest callbacks, so the events below are subscribed once for the dialog's life.
  const parent = useRef({ onClose, onMessage });
  parent.current = { onClose, onMessage };

  useEffect(() => {
    api
      .info()
      .then((i) => {
        setInfo(i);
        // No song end yet: the loop range is what there is to export.
        if (i.songEnd === null) {
          setRange("loop");
          setTail(false);
        }
        if (i.busy) setError("An export is already running.");
      })
      .catch((e) => setError(String(e)));
  }, []);

  // How a running export goes. Subscribed for as long as the dialog is open.
  useEffect(() => {
    const offs = [
      subscribe(api.onProgress((p) => setRunning(p))),
      subscribe(
        api.onDone((d) => {
          setRunning(null);
          const what = d.files.length === 1 ? fileName(d.files[0]) : `${d.files.length} files`;
          const clipped = d.clipped.length
            ? ` ${d.clipped.length === 1 ? fileName(d.clipped[0]) + " went" : d.clipped.length + " files went"} above full scale and clipped: export as 32-bit float to keep every peak.`
            : "";
          parent.current.onMessage({ text: `Exported ${what}, ${duration(d.seconds)} long, in ${d.took.toFixed(1)} s.${clipped}`, error: clipped !== "" });
          parent.current.onClose();
        }),
      ),
      subscribe(
        api.onCancelled((why) => {
          setRunning(null);
          parent.current.onMessage({ text: why, error: false });
          parent.current.onClose();
        }),
      ),
      subscribe(
        api.onFailed((e) => {
          setRunning(null);
          setError(e);
        }),
      ),
    ];
    return () => offs.forEach((off) => off());
  }, []);

  const heard = info?.tracks.filter((t) => t.heard).length ?? 0;
  const sig = info?.signature ?? [4, 4];
  const songOk = info?.songEnd != null;
  const loopOk = !!info && info.loopEnd > info.loopStart;
  const rangeOk = range === "song" ? songOk : loopOk;
  const canExport = !!info && !info.busy && !running && rangeOk && (master || (stems && heard > 0));

  function pickRange(r: Range) {
    setRange(r);
    // A loop is usually for looping elsewhere: no tail by default. The song rings out.
    setTail(r === "song");
  }

  async function run() {
    if (!info || !canExport) return;
    setError(null);
    let path: string | null;
    if (master) {
      path = await save({
        title: "Export project",
        defaultPath: `${info.project}.wav`,
        filters: [{ name: "WAV audio", extensions: ["wav"] }],
      }).catch(() => null);
    } else {
      const dir = await open({ directory: true, title: "Export stems to" }).catch(() => null);
      path = typeof dir === "string" ? dir : null;
    }
    if (!path) return;
    const seconds = range === "song" ? info.songSeconds : info.loopSeconds;
    setRunning({ phase: "settling", seconds: 0, range: seconds });
    try {
      await api.start({ range, format, master, stems, tail, path });
    } catch (e) {
      setRunning(null);
      setError(String(e));
    }
  }

  // Escape or a click outside while it runs cancels the export instead of hiding it.
  const close = () => (running ? void api.cancel() : onClose());

  const status = running
    ? running.phase === "settling"
      ? "Getting ready…"
      : running.phase === "tail"
        ? "Letting it ring out…"
        : `${duration(running.seconds)} of ${duration(running.range)}`
    : null;

  return (
    <Dialog title="Export project" onClose={close}>
      <fieldset className="field" disabled={!!running}>
        <legend>What to render</legend>
        <div className="seg" role="radiogroup">
          <label className={range === "song" ? "on" : ""} title={songOk ? undefined : "Put a clip on the Timeline or make a song in the Sampler first"}>
            <input type="radio" name="export-range" checked={range === "song"} disabled={!songOk} onChange={() => pickRange("song")} />
            The whole song{songOk && info ? ` · ${duration(info.songSeconds)}` : ""}
          </label>
          <label className={range === "loop" ? "on" : ""}>
            <input type="radio" name="export-range" checked={range === "loop"} disabled={!loopOk} onChange={() => pickRange("loop")} />
            The loop range{info && loopOk ? ` · ${bars(info.loopStart, sig)} to ${bars(info.loopEnd, sig)}` : ""}
          </label>
        </div>
      </fieldset>

      <fieldset className="field" disabled={!!running}>
        <legend>Files</legend>
        <div className="export-checks">
          <label className="export-check">
            <input type="checkbox" checked={master} onChange={(e) => setMaster(e.target.checked)} />
            The master
          </label>
          <label className="export-check">
            <input type="checkbox" checked={stems} disabled={heard === 0} onChange={(e) => setStems(e.target.checked)} />
            Stems, one per track{info ? ` (${heard})` : ""}
          </label>
        </div>
      </fieldset>

      <fieldset className="field" disabled={!!running}>
        <legend>Format</legend>
        <div className="seg" role="radiogroup">
          {FORMATS.map((f) => (
            <label key={f.id} className={format === f.id ? "on" : ""}>
              <input type="radio" name="export-format" checked={format === f.id} onChange={() => setFormat(f.id)} />
              {f.label}
            </label>
          ))}
        </div>
      </fieldset>

      <label className="export-check">
        <input type="checkbox" checked={tail} disabled={!!running} onChange={(e) => setTail(e.target.checked)} />
        Let reverbs and delays ring out after the end{info ? ` (up to ${info.tailSeconds} s)` : ""}
      </label>

      <p className="dim small">
        {info ? `${(info.rate / 1000).toFixed(info.rate % 1000 ? 1 : 0)} kHz WAV, the project's rate. ` : ""}
        Every track plays through its plugins, then the master level and limiter. Stems are each track after its
        level and pan, before the master{stems ? `, in a “stems” folder beside the master` : ""}; muted tracks are
        left out. Every file starts on the same frame.
        {format === "pcm16" ? " 16-bit files are dithered." : ""}
      </p>

      {running && (
        <div className="export-progress" role="status" aria-live="polite">
          <progress max={1} value={running.phase === "settling" ? 0 : running.range > 0 ? Math.min(1, running.seconds / running.range) : 1} />
          <span className="small mono">{status}</span>
        </div>
      )}
      {error && (
        <p className="small export-error" role="alert">
          {error}
        </p>
      )}

      <div className="dialog-foot">
        <button className="btn small" onClick={close}>
          {running ? "Stop export" : "Cancel"}
        </button>
        <button className="btn small primary" onClick={() => void run()} disabled={!canExport} autoFocus>
          {running ? "Exporting…" : "Export…"}
        </button>
      </div>
    </Dialog>
  );
}
