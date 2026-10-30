// The selected clip's settings, in a strip under the tracks: gain, fades,
// looping, and where it sits.
import type { ClipView } from "./api";
import { clipEnd, isLooped, position, setLooped } from "./edits";

type Props = {
  clip: ClipView;
  tempo: number;
  signature: [number, number];
  missing: boolean;
  onChange: (clip: ClipView, key: string) => void;
  onDelete: () => void;
};

const ms = (s: number) => Math.round(s * 1000);

export default function ClipInspector({ clip, tempo, signature, missing, onChange, onDelete }: Props) {
  const looped = isLooped(clip);
  const set = (patch: Partial<ClipView>, key: string) => onChange({ ...clip, ...patch }, `${key}:${clip.id}`);
  return (
    <section className="tl-inspector" aria-label="Clip">
      <strong className="tl-insp-name" title={clip.path}>
        {clip.name}
        {missing && <span className="tl-missing"> missing</span>}
      </strong>
      <span className="tl-insp-pos mono" title="Where it starts and ends (bar.beat.sixteenth)">
        {position(clip.start, signature)} – {position(clipEnd(clip, tempo), signature)}
      </span>
      <span className="mono dim" title="Length">
        {clip.length.toFixed(3)} s
      </span>
      <label className="tl-insp-field" title="Clip gain (double-click: 0 dB)">
        Gain
        <input
          type="range"
          min={-60}
          max={6}
          step={0.5}
          value={clip.gainDb}
          onChange={(e) => set({ gainDb: Number(e.target.value) }, "gain")}
          onDoubleClick={() => set({ gainDb: 0 }, "gain")}
        />
        <output className="mono">{clip.gainDb <= -60 ? "-∞" : `${clip.gainDb > 0 ? "+" : ""}${clip.gainDb.toFixed(1)} dB`}</output>
      </label>
      <label className="tl-insp-field" title="Fade in, in milliseconds (or drag the handle at the clip's top left)">
        Fade in
        <input
          type="number"
          min={0}
          step={5}
          value={ms(clip.fadeIn)}
          onChange={(e) => set({ fadeIn: Math.max(0, Math.min(Number(e.target.value) / 1000, clip.length - clip.fadeOut)) }, "fadein")}
        />
        <span className="dim">ms</span>
      </label>
      <label className="tl-insp-field" title="Fade out, in milliseconds (or drag the handle at the clip's top right)">
        Fade out
        <input
          type="number"
          min={0}
          step={5}
          value={ms(clip.fadeOut)}
          onChange={(e) => set({ fadeOut: Math.max(0, Math.min(Number(e.target.value) / 1000, clip.length - clip.fadeIn)) }, "fadeout")}
        />
        <span className="dim">ms</span>
      </label>
      <label className="tl-insp-check" title="Repeat the clip's audio; drag the top of its right edge to set how long">
        <input type="checkbox" checked={looped} onChange={(e) => onChange(setLooped(clip, e.target.checked), `loop:${clip.id}`)} />
        Loop
      </label>
      <button className="btn small danger" onClick={onDelete} title="Delete the clip (⌫)">
        Delete
      </button>
    </section>
  );
}
