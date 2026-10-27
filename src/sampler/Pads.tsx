// The 4×4 pads. Press to play (held for gate-mode pads), drag one pad onto
// another to swap them, drop samples from the Library or Finder onto them.
import { useEffect, useRef, useState } from "react";
import Wave from "./Wave";
import { PAD_KEYS, type PadView } from "./api";

interface Props {
  pads: PadView[];
  selected: number;
  played: number[];
  /** The pad a drag is over, to highlight it. */
  dropTarget: number | null;
  onSelect: (pad: number) => void;
  onPress: (pad: number) => void;
  onRelease: (pad: number) => void;
  onDragStart: (pad: number, x: number, y: number) => void;
}

export default function Pads({ pads, selected, played, dropTarget, onSelect, onPress, onRelease, onDragStart }: Props) {
  return (
    <section className="pads" aria-label="Pads">
      {pads.map((p) => (
        <Pad
          key={p.index}
          pad={p}
          selected={p.index === selected}
          played={played[p.index] ?? 0}
          over={dropTarget === p.index}
          onSelect={onSelect}
          onPress={onPress}
          onRelease={onRelease}
          onDragStart={onDragStart}
        />
      ))}
    </section>
  );
}

interface PadProps {
  pad: PadView;
  selected: boolean;
  played: number;
  over: boolean;
  onSelect: (pad: number) => void;
  onPress: (pad: number) => void;
  onRelease: (pad: number) => void;
  onDragStart: (pad: number, x: number, y: number) => void;
}

function Pad({ pad, selected, played, over, onSelect, onPress, onRelease, onDragStart }: PadProps) {
  const [flash, setFlash] = useState(0);
  const last = useRef(played);
  useEffect(() => {
    if (played !== last.current) {
      last.current = played;
      setFlash((f) => f + 1);
    }
  }, [played]);

  const key = PAD_KEYS[pad.index].toUpperCase();
  const loaded = !!pad.name && !pad.missing;
  const cls = ["pad", selected && "on", loaded && "loaded", pad.missing && "missing", over && "over", pad.settings.mute && "muted"]
    .filter(Boolean)
    .join(" ");
  const label = pad.name
    ? `Pad ${pad.index + 1}, ${pad.name}${pad.missing ? ", file missing" : ""}. Key ${key}.`
    : `Pad ${pad.index + 1}, empty. Key ${key}.`;

  return (
    <button
      className={cls}
      data-pad={pad.index}
      aria-label={label}
      aria-pressed={selected}
      title={pad.path ?? "Empty: drop a sample here, or pick one in the Library and press Enter"}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault(); // keep focus where it is, so Space stays play/stop
        onSelect(pad.index);
        onPress(pad.index);
        if (loaded) onDragStart(pad.index, e.clientX, e.clientY);
      }}
      onPointerUp={() => onRelease(pad.index)}
      onPointerLeave={(e) => e.buttons === 0 || onRelease(pad.index)}
      onKeyDown={(e) => {
        if ((e.key === "Enter" || e.key === " ") && !e.repeat) {
          e.preventDefault();
          e.stopPropagation();
          onSelect(pad.index);
          onPress(pad.index);
        }
      }}
      onKeyUp={(e) => {
        if (e.key === "Enter" || e.key === " ") onRelease(pad.index);
      }}
    >
      {flash > 0 && <span key={flash} className="pad-flash" aria-hidden="true" />}
      <span className="pad-top">
        <span className="pad-num">{pad.index + 1}</span>
        <kbd>{key}</kbd>
      </span>
      {loaded ? (
        <Wave className="pad-wave" peaks={pad.peaks} start={pad.settings.start} end={pad.settings.end} reverse={pad.settings.reverse} />
      ) : (
        <span className="pad-wave pad-empty" aria-hidden="true">{pad.missing ? "missing" : "+"}</span>
      )}
      <span className="pad-name">{pad.name ?? "Empty"}</span>
    </button>
  );
}
