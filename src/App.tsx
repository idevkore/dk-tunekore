// The main window: a Sampler / Timeline switch over the two views, both on
// the engine's one transport. Tab toggles between them, except while typing
// in a field or with a dialog open. The Sampler stays mounted while the
// Timeline shows, so its state and the prompts it owns (unsaved changes on
// quit, crash recovery, the window title) carry on.
import { useEffect, useState } from "react";
import SamplerView from "./sampler/SamplerView";
import Timeline from "./timeline/Timeline";

type MainView = "sampler" | "timeline";

const VIEWS: { id: MainView; label: string; icon: string }[] = [
  // A grid of pads, and lanes of clips.
  { id: "sampler", label: "Sampler", icon: "M2.5 2.5h4.5v4.5H2.5zM9 2.5h4.5v4.5H9zM2.5 9h4.5v4.5H2.5zM9 9h4.5v4.5H9z" },
  { id: "timeline", label: "Timeline", icon: "M2 4h7M11 4h3M2 8h3M7 8h7M2 12h9M13 12h1" },
];

/** Whether a key press goes to a field that takes text. */
function typing(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el) return false;
  if (el.tagName === "TEXTAREA" || el.tagName === "SELECT" || el.isContentEditable) return true;
  return el.tagName === "INPUT" && !["range", "checkbox", "radio", "button"].includes((el as HTMLInputElement).type);
}

export default function App() {
  const [view, setView] = useState<MainView>("sampler");

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Tab" || e.shiftKey || e.metaKey || e.ctrlKey || e.altKey) return;
      if (document.querySelector("dialog[open]") || typing(e.target)) return;
      e.preventDefault();
      setView((v) => (v === "sampler" ? "timeline" : "sampler"));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const switcher = <ViewSwitch view={view} onView={setView} />;
  return (
    <>
      <SamplerView active={view === "sampler"} switcher={switcher} />
      {view === "timeline" && <Timeline switcher={switcher} />}
    </>
  );
}

/** Two buttons in the top bar: which view the window shows. */
function ViewSwitch({ view, onView }: { view: MainView; onView: (v: MainView) => void }) {
  return (
    <div className="view-switch" role="group" aria-label="View">
      {VIEWS.map((v) => (
        <button
          key={v.id}
          className={view === v.id ? "on" : undefined}
          aria-pressed={view === v.id}
          onClick={() => onView(v.id)}
          aria-label={v.label}
          title={`${v.label} (Tab switches views)`}
        >
          <svg className="vs-icon" viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path d={v.icon} fill={v.id === "sampler" ? "currentColor" : "none"} stroke="currentColor" strokeWidth={v.id === "sampler" ? 0 : 1.6} strokeLinecap="round" />
          </svg>
          <span className="vs-label">{v.label}</span>
        </button>
      ))}
    </div>
  );
}
