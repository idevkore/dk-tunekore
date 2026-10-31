// The selected track's devices, in a strip under the tracks: what makes its
// sound (the Sampler, the clips, or a plugin instrument), then its effects
// left to right in the order the sound goes through them. Each plugin is a
// card: switch it off and on, open its window, move it along the chain,
// remove it; one that didn't load or failed says why and offers Reload.
import { useState } from "react";
import PluginPicker from "./PluginPicker";
import * as api from "./api";
import { chainOf, type CatalogEntry, type DeviceView, type PluginsView } from "./api";
import "./plugins.css";

type Track = { id: number; name: string; kind: "sampler" | "audio" | "instrument" };

type Props = {
  track: Track;
  plugins: PluginsView | null;
  onPlugins: (v: PluginsView) => void;
  onMessage: (text: string, error?: boolean) => void;
};

const SOURCE: Record<Track["kind"], string> = { sampler: "Sampler", audio: "Clips", instrument: "Instrument" };

export default function DeviceChain({ track, plugins, onPlugins, onMessage }: Props) {
  const [picking, setPicking] = useState<null | { kind: "instrument" | "effect"; index?: number }>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const { instrument, effects } = chainOf(plugins, track.id);

  const run = (p: Promise<PluginsView>, failure?: string) =>
    p.then(onPlugins).catch((e) => onMessage(failure ? `${failure}: ${e}` : String(e), true));

  const pick = (p: CatalogEntry) => {
    const what = picking;
    setPicking(null);
    if (!what) return;
    setBusy(`Loading ${p.name}…`);
    // A plugin can take a few seconds to load (Kontakt), and it loads on the UI's thread: say so first.
    api.afterPaint(() => {
      api
        .add(p, track.id, what.index)
        .then((v) => {
          onPlugins(v);
          onMessage(`${p.name} is on ${track.name}`);
        })
        .catch((e) => onMessage(`${p.name} didn't load: ${e}`, true))
        .finally(() => setBusy(null));
    });
  };

  return (
    <section className="dc" aria-label={`Devices on ${track.name}`}>
      <div className="dc-title">
        <strong>{track.name}</strong>
        <span className="dim">{busy ?? (effects.length === 1 ? "1 effect" : `${effects.length} effects`)}</span>
      </div>
      <div className="dc-row" role="list">
        {track.kind === "instrument" ? (
          instrument ? (
            <DeviceCard device={instrument} owner={track.name} run={run} />
          ) : (
            <button className="dc-add dc-slot" role="listitem" onClick={() => setPicking({ kind: "instrument" })} disabled={busy !== null}>
              + Instrument
            </button>
          )
        ) : (
          <div className="dc-fixed" role="listitem" title={track.kind === "sampler" ? "The pads make this track's sound" : "This track's clips make its sound"}>
            {SOURCE[track.kind]}
          </div>
        )}
        {effects.map((d, i) => (
          <DeviceCard key={d.id} device={d} owner={track.name} run={run} place={{ index: i, count: effects.length }} />
        ))}
        <button className="dc-add" onClick={() => setPicking({ kind: "effect" })} disabled={busy !== null} title="Add an effect at the end of the chain">
          + Effect
        </button>
      </div>
      {picking && (
        <PluginPicker kind={picking.kind} title={picking.kind === "instrument" ? `Instrument for ${track.name}` : `Add an effect to ${track.name}`} onPick={pick} onClose={() => setPicking(null)} />
      )}
    </section>
  );
}

type CardProps = {
  device: DeviceView;
  /** What it is on, for screen readers: a track's name, "pad 3". */
  owner: string;
  run: (p: Promise<PluginsView>, failure?: string) => void;
  /** An effect's place in the chain. */
  place?: { index: number; count: number };
};

/** One plugin: on/off, its window, remove, Reload when it is missing or failed; an effect's moves. */
export function DeviceCard({ device: d, owner, run, place }: CardProps) {
  const ok = d.status === "ok";
  const off = d.bypass || !ok;
  const status = d.status === "missing" ? "Missing" : d.status === "failed" ? "Failed" : d.bypass ? "Off" : null;
  return (
    <div className={`dc-card${off ? " off" : ""}${ok ? "" : " bad"}`} role="listitem" aria-label={`${d.name} on ${owner}`}>
      <div className="dc-card-top">
        <button
          className={`dc-power${d.bypass ? "" : " on"}`}
          aria-pressed={!d.bypass}
          aria-label={`${d.bypass ? "Switch on" : "Switch off"} ${d.name}`}
          title={d.bypass ? "Switch on" : d.role === "instrument" ? "Switch off (silent)" : "Switch off (the sound passes through)"}
          disabled={!ok}
          onClick={() => run(api.bypass(d.id, !d.bypass))}
        />
        <span className="dc-name" title={`${d.name} by ${d.vendor}`}>
          {d.name}
        </span>
        <button className="dc-x" aria-label={`Remove ${d.name}`} title="Remove it from the track" onClick={() => run(api.remove(d.id))}>
          ×
        </button>
      </div>
      <span className="dc-vendor">{d.vendor}</span>
      {status && (
        <span className={`dc-status${ok ? "" : " warn"}`} title={d.error ?? undefined}>
          {status}
          {d.error && !ok ? `: ${d.error}` : ""}
        </span>
      )}
      <div className="dc-actions">
        {ok ? (
          <button
            className={`btn small${d.editorOpen ? " on" : ""}`}
            disabled={!d.hasEditor}
            title={d.hasEditor ? "Open its window" : "It has no window of its own"}
            onClick={() => run(api.editor(d.id, true))}
          >
            {d.editorOpen ? "Show" : "Edit"}
          </button>
        ) : (
          <button className="btn small" title="Load it again, from the sound last saved" onClick={() => run(api.reload(d.id), `${d.name} didn't load`)}>
            Reload
          </button>
        )}
        {place && place.count > 1 && (
          <span className="group-btns">
            <button className="btn icon small" aria-label={`Move ${d.name} earlier`} title="Earlier in the chain" disabled={place.index === 0} onClick={() => run(api.move(d.id, place.index - 1))}>
              ‹
            </button>
            <button
              className="btn icon small"
              aria-label={`Move ${d.name} later`}
              title="Later in the chain"
              disabled={place.index === place.count - 1}
              onClick={() => run(api.move(d.id, place.index + 1))}
            >
              ›
            </button>
          </span>
        )}
        {d.latency > 0 && (
          <span className="dc-latency mono" title="The delay it adds, in samples (delay compensation comes with the Mixer)">
            {d.latency}
          </span>
        )}
      </div>
    </div>
  );
}
