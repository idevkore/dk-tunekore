// A waveform drawn from peaks (0..1), mirrored around the middle, as SVG.

interface Props {
  peaks: number[];
  /** Shade outside start..end (fractions 0..1). */
  start?: number;
  end?: number;
  /** Draw it backwards (a reversed pad). */
  reverse?: boolean;
  className?: string;
}

export default function Wave({ peaks, start = 0, end = 1, reverse = false, className }: Props) {
  const n = peaks.length;
  if (n === 0) return <svg className={className} viewBox="0 0 100 40" preserveAspectRatio="none" aria-hidden="true" />;
  const pts = reverse ? [...peaks].reverse() : peaks;
  let top = "";
  let bottom = "";
  for (let i = 0; i < n; i++) {
    const x = (i / (n - 1)) * 100;
    const h = Math.max(0.4, pts[i] * 19);
    top += `${i === 0 ? "M" : "L"}${x.toFixed(2)},${(20 - h).toFixed(2)}`;
    bottom = `L${x.toFixed(2)},${(20 + h).toFixed(2)}` + bottom;
  }
  const s = (reverse ? 1 - end : start) * 100;
  const e = (reverse ? 1 - start : end) * 100;
  return (
    <svg className={className} viewBox="0 0 100 40" preserveAspectRatio="none" aria-hidden="true">
      <path d={`${top}${bottom}Z`} className="wave-fill" />
      {s > 0 && <rect x={0} y={0} width={s} height={40} className="wave-trim" />}
      {e < 100 && <rect x={e} y={0} width={100 - e} height={40} className="wave-trim" />}
    </svg>
  );
}
