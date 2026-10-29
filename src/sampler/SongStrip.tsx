// Song mode: a chain of patterns to play in order. Add patterns with the
// letter buttons, click one in the chain to take it out, and switch Song
// on to play through the chain instead of looping one pattern.
import { PATTERN_NAMES } from "./api";

interface Props {
  chain: number[];
  songMode: boolean;
  songLoop: boolean;
  /** The chain entry playing now. After song mode goes off it plays to its end. */
  position: number | null;
  /** Song mode was switched on or off while playing, and lands at the end of the pattern. */
  queued: boolean;
  patternsUsed: boolean[];
  /** Seconds one pattern lasts, for the song's length. */
  passSeconds: number;
  onChange: (chain: number[], songMode: boolean, songLoop: boolean) => void;
}

const MAX = 64;

function clock(s: number) {
  const m = Math.floor(s / 60);
  return `${m}:${String(Math.round(s - m * 60)).padStart(2, "0")}`;
}

export default function SongStrip({ chain, songMode, songLoop, position, queued, patternsUsed, passSeconds, onChange }: Props) {
  const set = (c: number[], mode = songMode, loop = songLoop) => onChange(c, mode, loop);
  return (
    <div className={`song${songMode ? " on" : ""}`} role="group" aria-label="Song">
      <button
        className={`song-toggle${songMode ? " on" : ""}${queued ? " queued" : ""}`}
        aria-pressed={songMode}
        onPointerDown={(e) => e.preventDefault()}
        onClick={() => set(chain, !songMode)}
        disabled={chain.length === 0 && !songMode}
        title={
          chain.length === 0
            ? "Add patterns to the song first"
            : queued
              ? `The song ${songMode ? "starts" : "stops"} at the end of the pattern`
              : "Play the song: the patterns below in order, instead of looping one"
        }
      >
        Song
      </button>
      <ol className="chain" aria-label="Song order">
        {chain.length === 0 && <li className="chain-empty">Add patterns to build a song →</li>}
        {chain.map((b, i) => (
          <li key={i}>
            <button
              className={`chip${position === i ? " now" : ""}`}
              onPointerDown={(e) => e.preventDefault()}
              onClick={() => set(chain.filter((_, j) => j !== i))}
              title={`Part ${i + 1}: pattern ${PATTERN_NAMES[b]}. Click to take it out.`}
              aria-label={`Part ${i + 1}, pattern ${PATTERN_NAMES[b]}; remove`}
            >
              {PATTERN_NAMES[b]}
            </button>
          </li>
        ))}
      </ol>
      <span className="song-add" aria-label="Add a pattern to the song">
        {PATTERN_NAMES.map((n, b) => (
          <button
            key={n}
            className={`add${patternsUsed[b] ? "" : " unused"}`}
            onPointerDown={(e) => e.preventDefault()}
            onClick={() => set([...chain, b].slice(0, MAX))}
            disabled={chain.length >= MAX}
            title={`Add pattern ${n} to the end of the song${patternsUsed[b] ? "" : " (it's empty)"}`}
          >
            +{n}
          </button>
        ))}
      </span>
      <label className="loop" title="At the end of the song, start again">
        <input type="checkbox" checked={songLoop} onChange={(e) => set(chain, songMode, e.target.checked)} />
        Loop
      </label>
      {chain.length > 0 && (
        <span className="dim small mono song-len">
          {chain.length} part{chain.length > 1 ? "s" : ""} · {clock(chain.length * passSeconds)}
        </span>
      )}
      {chain.length > 0 && (
        <button className="link" onClick={() => set([], false)}>
          Clear
        </button>
      )}
    </div>
  );
}
