import { useEffect, useRef } from "react";

interface Props {
  peaks: number[];
  color: string;
  /** 0..1 of the song, or null for no playhead. */
  playhead: number | null;
  playing?: boolean;
  dim?: boolean;
  /** Fills in from the left up to this point (0..1), for stems still being made. */
  reveal?: number;
  height?: number;
  label: string;
  onSeek?: (fraction: number) => void;
}

/** A mirrored peak waveform on a canvas, with a playhead. Click to seek. */
export default function Waveform({ peaks, color, playhead, playing, dim, reveal = 1, height = 56, label, onSeek }: Props) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const c = canvas.current;
    const b = box.current;
    if (!c || !b) return;
    const draw = () => {
      const dpr = window.devicePixelRatio || 1;
      const w = Math.max(1, b.clientWidth);
      c.width = Math.round(w * dpr);
      c.height = Math.round(height * dpr);
      const g = c.getContext("2d");
      if (!g) return;
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      g.clearRect(0, 0, w, height);
      const mid = height / 2;
      const n = peaks.length;
      if (n === 0) return;
      const bar = 2;
      const gap = 1;
      const bars = Math.floor(w / (bar + gap));
      g.fillStyle = color;
      g.globalAlpha = dim ? 0.28 : 0.92;
      for (let i = 0; i < bars; i++) {
        const t = i / bars;
        if (t > reveal) break;
        const lo = Math.floor(t * n);
        const hi = Math.max(lo + 1, Math.floor(((i + 1) / bars) * n));
        let p = 0;
        for (let k = lo; k < hi && k < n; k++) p = Math.max(p, peaks[k]);
        const h = Math.max(1, Math.sqrt(p) * (mid - 2)); // sqrt: quiet parts stay visible
        g.fillRect(i * (bar + gap), mid - h, bar, h * 2);
      }
    };
    draw();
    const ro = new ResizeObserver(draw);
    ro.observe(b);
    return () => ro.disconnect();
  }, [peaks, color, dim, reveal, height]);

  function click(e: React.MouseEvent<HTMLDivElement>) {
    if (!onSeek || !box.current) return;
    const r = box.current.getBoundingClientRect();
    onSeek(Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)));
  }

  function key(e: React.KeyboardEvent<HTMLDivElement>) {
    if (!onSeek || playhead === null) return;
    if (e.key === "ArrowRight") onSeek(Math.min(1, playhead + 0.02));
    else if (e.key === "ArrowLeft") onSeek(Math.max(0, playhead - 0.02));
    else return;
    e.preventDefault();
  }

  return (
    <div
      ref={box}
      className={"wave" + (onSeek ? " seekable" : "")}
      style={{ height }}
      onClick={click}
      onKeyDown={key}
      role={onSeek ? "slider" : "img"}
      tabIndex={onSeek ? 0 : undefined}
      aria-label={label}
      aria-valuemin={onSeek ? 0 : undefined}
      aria-valuemax={onSeek ? 100 : undefined}
      aria-valuenow={onSeek && playhead !== null ? Math.round(playhead * 100) : undefined}
    >
      <canvas ref={canvas} style={{ width: "100%", height }} />
      {playhead !== null && (
        <span className={"playhead" + (playing ? " on" : "")} style={{ left: `${playhead * 100}%` }} />
      )}
    </div>
  );
}
