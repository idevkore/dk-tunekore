// Export the pattern as a WAV file, repeated a chosen number of times.
import { useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { exportPattern, exportStems } from "./api";
import Dialog from "./Dialog";

const LOOPS = [1, 2, 4, 8];

interface Props {
  name: string;
  steps: number;
  tempo: number;
  disabled: boolean;
  onClose: () => void;
  onDone: (path: string) => void;
  /** Renders the pattern onto the first empty pad. */
  onBounce: (loops: number, song: boolean) => Promise<void>;
  /** Whether a song chain exists, to offer exporting the whole song. */
  hasSong: boolean;
  onError: (e: unknown) => void;
}

export default function ExportDialog({ name, steps, tempo, disabled, onClose, onDone, onBounce, onError, hasSong }: Props) {
  const [loops, setLoops] = useState(4);
  const [song, setSong] = useState(hasSong);
  const [busy, setBusy] = useState(false);
  const seconds = (loops * steps * 15) / tempo;

  async function stems() {
    const dir = await open({ directory: true, title: "Export stems to" }).catch(() => null);
    if (typeof dir !== "string") return;
    setBusy(true);
    try {
      const out = await exportStems(dir, loops, song);
      onDone(`${out.length} stems to ${dir}`);
      onClose();
    } catch (e) {
      onError(e);
    } finally {
      setBusy(false);
    }
  }

  async function run() {
    const path = await save({
      title: "Export pattern",
      defaultPath: `${name}.wav`,
      filters: [{ name: "WAV audio", extensions: ["wav"] }],
    }).catch(() => null);
    if (!path) return;
    setBusy(true);
    try {
      const out = await exportPattern(path, loops, song);
      onDone(out.path);
      onClose();
    } catch (e) {
      onError(e);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog title="Export or bounce" onClose={onClose}>
      {hasSong && (
        <fieldset className="field">
          <legend>What to render</legend>
          <div className="seg" role="radiogroup">
            <label className={song ? "on" : ""}>
              <input type="radio" name="what" checked={song} onChange={() => setSong(true)} />
              The whole song
            </label>
            <label className={!song ? "on" : ""}>
              <input type="radio" name="what" checked={!song} onChange={() => setSong(false)} />
              This pattern
            </label>
          </div>
        </fieldset>
      )}
      {!song && <fieldset className="field">
        <legend>Repeat the pattern</legend>
        <div className="seg" role="radiogroup">
          {LOOPS.map((n) => (
            <label key={n} className={loops === n ? "on" : ""}>
              <input type="radio" name="loops" value={n} checked={loops === n} onChange={() => setLoops(n)} />
              {n}×
            </label>
          ))}
        </div>
      </fieldset>}
      <p className="dim small">
        {song
          ? "The song plays through once, as a 48 kHz WAV."
          : `${seconds.toFixed(1)} s of audio, 48 kHz WAV. It ends exactly on the last step, so it loops cleanly.`}
        Stems are one WAV per pad, all the same length, to line up in another DAW. Bounce puts the
        mix on the next empty pad, to chop, pitch or reverse like any sample.
      </p>
      <div className="dialog-foot">
        <button className="btn small" onClick={onClose}>Cancel</button>
        <button
          className="btn small"
          disabled={busy || disabled}
          onClick={() => {
            setBusy(true);
            void onBounce(loops, song).finally(() => {
              setBusy(false);
              onClose();
            });
          }}
        >
          Bounce to a pad
        </button>
        <button className="btn small" disabled={busy || disabled} onClick={() => void stems()}>
          Export stems…
        </button>
        <button className="btn small primary" onClick={() => void run()} disabled={busy || disabled} autoFocus>
          {busy ? "Exporting…" : "Export WAV…"}
        </button>
      </div>
    </Dialog>
  );
}
