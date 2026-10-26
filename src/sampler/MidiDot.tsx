// A small light in the top bar: blinks when a MIDI controller plays a pad, and says whether one is connected.
import { useEffect, useRef, useState } from "react";
import { midiInputs, onMidiActivity, subscribe } from "./api";

export default function MidiDot() {
  const [names, setNames] = useState<string[]>([]);
  const [blink, setBlink] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);

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

  const title = names.length ? `MIDI: ${names.join(", ")}` : "No MIDI controller found: plug one in and it connects by itself";
  return (
    <span className={`midi-dot${names.length ? " on" : ""}${blink ? " blink" : ""}`} title={title} role="status" aria-label={title}>
      <i aria-hidden="true" />
      MIDI
    </span>
  );
}
