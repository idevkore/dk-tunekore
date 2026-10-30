// One track's header beside its lane: colour, name, mute, solo, arm (a
// placeholder until recording, #40), volume, pan and a meter, and a handle
// at the foot to change the track's height.
import { useEffect, useRef, useState } from "react";
import { COLOURS, type TrackMix, type TrackView } from "./api";

/** Keeps the pointer's events coming here through a drag (a pointer that can't be captured just isn't). */
function capture(e: React.PointerEvent<Element>) {
  try {
    e.currentTarget.setPointerCapture(e.pointerId);
  } catch {
    // a synthetic or already-released pointer
  }
}

const MIN_DB = -60;
const MAX_DB = 6;

/** Linear gain to the fader's dB, -60 meaning off. */
export const gainToDb = (g: number) => (g <= 0 ? MIN_DB : Math.max(MIN_DB, Math.min(MAX_DB, 20 * Math.log10(g))));
export const dbToGain = (db: number) => (db <= MIN_DB ? 0 : Math.pow(10, db / 20));

const dbText = (db: number) => (db <= MIN_DB ? "-∞" : `${db > 0 ? "+" : ""}${db.toFixed(1)}`);
const panText = (p: number) => (Math.abs(p) < 0.005 ? "C" : `${Math.round(Math.abs(p) * 100)}${p < 0 ? "L" : "R"}`);

/** Peak 0..1 to a 0..1 position on a -60..0 dB scale. */
const meterPosition = (peak: number) => (peak <= 0 ? 0 : Math.min(1, Math.max(0, (20 * Math.log10(peak) + 60) / 60)));

type Props = {
  track: TrackView;
  height: number;
  selected: boolean;
  /** Latest peak, 0..1. */
  level: number;
  onSelect: () => void;
  onMix: (mix: TrackMix) => void;
  onRename: (name: string) => void;
  onColour: (colour: number) => void;
  onRemove: () => void;
  /** While the foot is dragged, and once more (`done`) on release. */
  onHeight: (height: number, done: boolean) => void;
};

export default function TrackHeader({ track, height, selected, level, onSelect, onMix, onRename, onColour, onRemove, onHeight }: Props) {
  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState(track.name);
  const resize = useRef<{ y0: number; h0: number } | null>(null);
  const mix = track.mix;
  const db = gainToDb(mix.volume);
  const sampler = track.kind === "sampler";
  const compact = height < 56;

  useEffect(() => setName(track.name), [track.name]);

  const commit = () => {
    setRenaming(false);
    if (name.trim() && name !== track.name) onRename(name.trim());
    else setName(track.name);
  };

  return (
    <div
      className={`tl-head${selected ? " on" : ""}${mix.mute ? " muted" : ""}`}
      style={{ height }}
      data-track={track.id}
      onPointerDown={(e) => {
        if ((e.target as HTMLElement).closest("input, button")) return;
        onSelect();
      }}
    >
      <button
        className="tl-colour"
        style={{ background: COLOURS[track.colour % COLOURS.length] }}
        aria-label={`Colour of ${track.name}`}
        title="Change the track's colour"
        onClick={() => onColour((track.colour + 1) % COLOURS.length)}
      />
      <div className="tl-head-main">
        <div className="tl-head-top">
          {renaming ? (
            <input
              className="tl-name-input"
              value={name}
              autoFocus
              aria-label="Track name"
              onChange={(e) => setName(e.target.value)}
              onBlur={commit}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                if (e.key === "Escape") {
                  setName(track.name);
                  setRenaming(false);
                }
              }}
            />
          ) : (
            <span className="tl-name" title={`${track.name}${sampler ? "" : "\nDouble-click to rename"}`} onDoubleClick={() => !sampler && setRenaming(true)}>
              {track.name}
            </span>
          )}
          <button className={`tl-ms${mix.mute ? " on" : ""}`} aria-pressed={mix.mute} title="Mute" aria-label={`Mute ${track.name}`} onClick={() => onMix({ ...mix, mute: !mix.mute })}>
            M
          </button>
          <button className={`tl-ms solo${mix.solo ? " on" : ""}`} aria-pressed={mix.solo} title="Solo" aria-label={`Solo ${track.name}`} onClick={() => onMix({ ...mix, solo: !mix.solo })}>
            S
          </button>
          <button className="tl-ms arm" disabled title="Arm for recording: comes with recording into the Timeline" aria-label={`Arm ${track.name} (not yet)`}>
            ●
          </button>
          {!sampler && (
            <button className="tl-x" title="Delete the track (undo brings it back)" aria-label={`Delete ${track.name}`} onClick={onRemove}>
              ×
            </button>
          )}
        </div>
        {!compact && (
          <div className="tl-faders">
            <label className="tl-fader" title={`Volume ${dbText(db)} dB (double-click: 0 dB)`}>
              <span className="sr">Volume</span>
              <input
                type="range"
                min={MIN_DB}
                max={MAX_DB}
                step={0.5}
                value={db}
                onChange={(e) => onMix({ ...mix, volume: dbToGain(Number(e.target.value)) })}
                onDoubleClick={() => onMix({ ...mix, volume: 1 })}
              />
              <output className="mono">{dbText(db)}</output>
            </label>
            <label className="tl-fader pan" title={`Pan ${panText(mix.pan)} (double-click: centre)`}>
              <span className="sr">Pan</span>
              <input type="range" min={-1} max={1} step={0.02} value={mix.pan} onChange={(e) => onMix({ ...mix, pan: Number(e.target.value) })} onDoubleClick={() => onMix({ ...mix, pan: 0 })} />
              <output className="mono">{panText(mix.pan)}</output>
            </label>
          </div>
        )}
        <div className="tl-meter" aria-hidden="true">
          <div style={{ transform: `scaleX(${meterPosition(level)})` }} />
        </div>
      </div>
      <div
        className="tl-resize"
        title="Drag to change the track's height"
        onPointerDown={(e) => {
          e.preventDefault();
          e.stopPropagation();
          capture(e);
          resize.current = { y0: e.clientY, h0: height };
        }}
        onPointerMove={(e) => {
          if (resize.current) onHeight(resize.current.h0 + e.clientY - resize.current.y0, false);
        }}
        onPointerUp={(e) => {
          if (resize.current) onHeight(resize.current.h0 + e.clientY - resize.current.y0, true);
          resize.current = null;
        }}
        onPointerCancel={() => (resize.current = null)}
      />
    </div>
  );
}
