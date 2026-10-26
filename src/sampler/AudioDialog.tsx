// Audio settings: which output device to play on, and a test tone to check it.
import { useEffect, useState } from "react";
import { engineVersion, getMaster, listOutputs, setMaster, startTone, stopTone, type OutputDevice } from "../engine";
import { chooseInput, chosenInput, currentOutput, inputDevices, midiInputs, setOutput, type InputDevice, type MidiInputInfo } from "./api";
import Dialog from "./Dialog";

const DEFAULT = "";

export default function AudioDialog({ onClose, onError }: { onClose: () => void; onError: (e: unknown) => void }) {
  const [devices, setDevices] = useState<OutputDevice[]>([]);
  const [device, setDevice] = useState(DEFAULT);
  const [tone, setTone] = useState(false);
  const [version, setVersion] = useState("");
  const [master, setMasterDb] = useState(0);
  const [inputs, setInputs] = useState<InputDevice[]>([]);
  const [input, setInput] = useState(chosenInput() ?? DEFAULT);
  const [midi, setMidi] = useState<MidiInputInfo[]>([]);

  useEffect(() => {
    listOutputs().then(setDevices).catch(onError);
    currentOutput().then((d) => setDevice(d ?? DEFAULT)).catch(() => {});
    engineVersion().then(setVersion).catch(() => {});
    getMaster().then(setMasterDb).catch(() => {});
    inputDevices().then(setInputs).catch(() => {});
    const poll = () => midiInputs().then(setMidi).catch(() => {});
    poll();
    const every = setInterval(poll, 2000); // a controller plugged in while this is open shows up
    return () => {
      clearInterval(every);
      void stopTone().catch(() => {});
    };
  }, [onError]);

  async function choose(id: string) {
    setDevice(id);
    try {
      await setOutput(id === DEFAULT ? null : id);
      if (tone) await startTone(id === DEFAULT ? null : id, 440);
    } catch (e) {
      onError(e);
    }
  }

  async function toggleTone() {
    try {
      if (tone) await stopTone();
      else await startTone(device === DEFAULT ? null : device, 440);
      setTone(!tone);
    } catch (e) {
      onError(e);
    }
  }

  const sys = devices.find((d) => d.isDefault);
  const sysIn = inputs.find((d) => d.isDefault);
  const connected = midi.filter((m) => m.connected);
  return (
    <Dialog title="Audio" onClose={onClose}>
      <label className="field">
        <span>Output</span>
        <select value={device} onChange={(e) => void choose(e.target.value)}>
          <option value={DEFAULT}>System default{sys ? ` (${sys.name})` : ""}</option>
          {devices.map((d) => (
            <option key={d.id} value={d.id}>
              {d.name} · {d.channels} ch · {(d.preferredRate / 1000).toFixed(1)} kHz
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>
          Master level <span className="mono">{master <= -60 ? "off" : `${master > 0 ? "+" : ""}${master.toFixed(1)} dB`}</span>
        </span>
        <input
          type="range"
          min={-60}
          max={6}
          step={0.5}
          value={master}
          onChange={(e) => {
            const db = Number(e.target.value);
            setMasterDb(db);
            setMaster(db).catch(onError);
          }}
          onDoubleClick={() => {
            setMasterDb(0);
            setMaster(0).catch(onError);
          }}
        />
        <span className="dim small">A limiter after it keeps the output under -0.3 dBFS, so a full kit never clips. Double-click to reset.</span>
      </label>
      <div className="field-row">
        <button className={`btn small${tone ? " primary" : ""}`} onClick={() => void toggleTone()}>
          {tone ? "Stop test tone" : "Play test tone"}
        </button>
        <span className="dim small">A quiet 440 Hz tone, to check the output.</span>
      </div>
      <label className="field">
        <span>Input (for sampling)</span>
        <select
          value={input}
          onChange={(e) => {
            setInput(e.target.value);
            chooseInput(e.target.value === DEFAULT ? null : e.target.value);
          }}
        >
          <option value={DEFAULT}>System default{sysIn ? ` (${sysIn.name})` : ""}</option>
          {inputs.map((d) => (
            <option key={d.id} value={d.id}>
              {d.name} · {d.channels} ch · {(d.preferredRate / 1000).toFixed(1)} kHz
            </option>
          ))}
        </select>
      </label>
      <p className="field-help">The first two channels of the input are recorded. Pick a pad, then “Sample from input” in its settings.</p>
      <div className="field">
        <span>MIDI input</span>
        {connected.length ? (
          <ul className="midi-list" aria-label="Connected MIDI controllers">
            {connected.map((m) => (
              <li key={m.name}>{m.name}</li>
            ))}
          </ul>
        ) : (
          <span className="dim">None found: plug one in, it connects by itself.</span>
        )}
        <p className="field-help">Notes C1 to D♯2 (36 to 51) play pads 1 to 16, on any channel.</p>
      </div>
      <div className="dialog-foot">
        <span className="dim small mono">engine {version}</span>
        <button className="btn small primary" onClick={onClose} autoFocus>
          Done
        </button>
      </div>
    </Dialog>
  );
}
