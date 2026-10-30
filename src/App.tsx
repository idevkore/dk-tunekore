// The main window: a Sampler / Timeline switch over the two views, both on
// the engine's one transport, and the window's status line. Both views stay
// mounted, the hidden one hidden: the Sampler's state and the prompts it
// owns (unsaved changes on quit, crash recovery, the window title) carry on,
// and so do the Timeline's Library, selection and view.
//
// Switching: ⌘1 is the Sampler and ⌘2 the Timeline (Ctrl elsewhere), from the
// keyboard, the View menu or the switch buttons; ⌘3 is kept for the Clip
// Grid. Tab also flips between them, but only while nothing has focus; the
// flip then focuses the shown view's switch, so the next Tab moves into the
// view's controls. On any control Tab moves focus as usual.
import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import SamplerView from "./sampler/SamplerView";
import Timeline from "./timeline/Timeline";
import ThemePicker from "./theme/ThemePicker";
import type { Message } from "./StatusBar";

type MainView = "sampler" | "timeline";

/** The key that flips between the views while nothing has focus. */
const FLIP_KEY = "Tab";
/** How long a message that isn't an error stays up. */
const MESSAGE_MS = 4000;

const VIEWS: { id: MainView; label: string; key: string; icon: string }[] = [
  // A grid of pads, and lanes of clips.
  { id: "sampler", label: "Sampler", key: "1", icon: "M2.5 2.5h4.5v4.5H2.5zM9 2.5h4.5v4.5H9zM2.5 9h4.5v4.5H2.5zM9 9h4.5v4.5H9z" },
  { id: "timeline", label: "Timeline", key: "2", icon: "M2 4h7M11 4h3M2 8h3M7 8h7M2 12h9M13 12h1" },
];

const isMac = navigator.platform.toLowerCase().includes("mac");
const mod = isMac ? "⌘" : "Ctrl+";

export default function App() {
  const [view, setView] = useState<MainView>("sampler");
  const [message, setMessage] = useState<Message | null>(null);
  // Bumped by a switch from the keyboard or the menu: focus moves to the shown view's switch.
  const [focusSwitch, setFocusSwitch] = useState(0);

  const viewRef = useRef(view);
  viewRef.current = view;

  // ⌘1, ⌘2 and the menu: the view already showing stays as it is, focus included.
  const show = useCallback((v: MainView, byKey: boolean) => {
    if (v === viewRef.current) return;
    setView(v);
    if (byKey) setFocusSwitch((n) => n + 1);
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (document.querySelector("dialog[open]")) return;
      if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey) {
        const v = VIEWS.find((x) => x.key === e.key);
        if (v) {
          e.preventDefault();
          show(v.id, true);
        }
        return;
      }
      if (e.key !== FLIP_KEY || e.shiftKey || e.altKey || e.repeat) return;
      const at = document.activeElement;
      if (at && at !== document.body && at !== document.documentElement) return;
      e.preventDefault();
      setView((v) => (v === "sampler" ? "timeline" : "sampler"));
      setFocusSwitch((n) => n + 1);
    };
    window.addEventListener("keydown", onKey);
    // The View menu's Sampler and Timeline items.
    let off: (() => void) | undefined;
    let gone = false;
    try {
      listen<string>("view:show", (e) => {
        // As with the keys: a dialog open in the Sampler stays on screen.
        if (document.querySelector("dialog[open]")) return;
        const v = VIEWS.find((x) => x.id === e.payload);
        if (v) show(v.id, true);
      })
        .then((u) => (gone ? u() : (off = u)))
        .catch(() => {});
    } catch {
      // not in Tauri (the dev mock): the keys still work
    }
    return () => {
      gone = true;
      off?.();
      window.removeEventListener("keydown", onKey);
    };
  }, [show]);

  // After a switch from the keyboard or the menu, focus the shown view's switch,
  // so focus isn't left in the hidden view and Tab carries on from there.
  useEffect(() => {
    if (focusSwitch === 0) return;
    document.querySelector<HTMLElement>(".app:not([hidden]) .view-switch button[aria-pressed=true]")?.focus({ preventScroll: true });
  }, [focusSwitch, view]);

  // A message that isn't an error goes away by itself.
  useEffect(() => {
    if (!message || message.error) return;
    const t = setTimeout(() => setMessage(null), MESSAGE_MS);
    return () => clearTimeout(t);
  }, [message]);

  const switcher = (
    <>
      <ViewSwitch view={view} onView={(v) => show(v, false)} />
      <ThemePicker />
    </>
  );
  return (
    <>
      <SamplerView active={view === "sampler"} switcher={switcher} message={message} onMessage={setMessage} />
      <Timeline active={view === "timeline"} switcher={switcher} message={message} onMessage={setMessage} />
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
          aria-keyshortcuts={isMac ? `Meta+${v.key}` : `Control+${v.key}`}
          title={`${v.label} (${mod}${v.key})`}
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
