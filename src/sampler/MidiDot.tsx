// A small light in the top bar: blinks when a MIDI controller plays a pad,
// and says whether one is connected. Clicking it turns MIDI Learn on and off
// (src/midi/learn.tsx): the top bar has no room for another button at the
// narrowest window.
import { useEffect, useRef, useState } from "react";
import { midiInputs, onMidiActivity, subscribe } from "./api";
import { useMidi } from "../midi/learn";

export default function MidiDot() {
  const [names, setNames] = useState<string[]>([]);
  const [blink, setBlink] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const learn = useMidi();

  useEffect(() => {
    let live = true;
    const poll = () =>
      midiInputs()
        .then((l) => live && setNames(l.filter((i) => i.connected).map((i) => i.name)))
        .catch(() => {});
    poll();
    const every = setInterval(poll, 3000);
    const off = subscribe(
      onMidiActivity(() => {
        setBlink(true);
        clearTimeout(timer.current);
        timer.current = setTimeout(() => setBlink(false), 90);
      }),
    );
    return () => {
      live = false;
      clearInterval(every);
      clearTimeout(timer.current);
      off();
    };
  }, []);

  const status = names.length ? `MIDI: ${names.join(", ")}` : "No MIDI controller found: plug one in and it connects by itself";
  const on = learn?.mode ?? false;
  return (
    <button
      type="button"
      className={`midi-dot${names.length ? " on" : ""}${blink ? " blink" : ""}${on ? " learn" : ""}`}
      aria-pressed={on}
      aria-label={`MIDI Learn. ${status}`}
      title={`${status}.\nClick for MIDI Learn: click a control, then move a knob or fader to map it.`}
      onClick={() => learn?.setMode(!on)}
    >
      <i aria-hidden="true" />
      <span className="wide-only" aria-hidden="true">
        {on ? "LEARN" : "MIDI"}
      </span>
    </button>
  );
}
