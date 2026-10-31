// The theme button in the top bar: a small menu of the colour themes, dark
// then light, each with a swatch of its ground, accent and highlight. A pick
// applies at once and is remembered for the next launch.
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { applyTheme, THEMES, useTheme, type Theme } from "./theme";

const GROUPS: { tone: Theme["tone"]; label: string }[] = [
  { tone: "dark", label: "Dark" },
  { tone: "light", label: "Light" },
];

export default function ThemePicker() {
  const theme = useTheme();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => ref.current?.contains(e.target as Node) || setOpen(false);
    window.addEventListener("pointerdown", away);
    ref.current?.querySelector<HTMLButtonElement>("[role=menuitemradio][aria-checked=true]")?.focus();
    return () => window.removeEventListener("pointerdown", away);
  }, [open]);

  const close = () => {
    setOpen(false);
    button.current?.focus();
  };

  // The menu keeps its keys: arrows move between themes, Escape closes.
  // Nothing reaches the views' own shortcuts (pads, Space) while it is open.
  const onKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const items = [...(ref.current?.querySelectorAll<HTMLButtonElement>("[role=menuitemradio]") ?? [])];
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const go = (i: number) => items[(i + items.length) % items.length]?.focus();
    const keys: Record<string, () => void> = {
      ArrowDown: () => go(at + 1),
      ArrowUp: () => go(at - 1),
      Home: () => go(0),
      End: () => go(items.length - 1),
      Escape: close,
      Tab: () => setOpen(false),
    };
    const f = keys[e.key];
    if (f) {
      if (e.key !== "Tab") e.preventDefault();
      f();
    }
    e.stopPropagation();
  };

  return (
    <div className="theme-picker" ref={ref} onKeyDown={open ? onKey : undefined}>
      <button
        ref={button}
        className="theme-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Colour theme: ${theme.name}`}
        title={`Colour theme: ${theme.name}`}
        onClick={() => setOpen(!open)}
      >
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <path d="M8 2a6 6 0 0 1 0 12z" fill="currentColor" />
        </svg>
      </button>
      {open && (
        <div className="menu theme-menu" role="menu" aria-label="Colour theme">
          {GROUPS.map((g) => (
            <div key={g.tone} role="group" aria-label={g.label}>
              <div className="theme-group" aria-hidden="true">
                {g.label}
              </div>
              {THEMES.filter((t) => t.tone === g.tone).map((t) => (
                <button
                  key={t.id}
                  role="menuitemradio"
                  aria-checked={t.id === theme.id}
                  onClick={() => {
                    applyTheme(t.id);
                    close();
                  }}
                >
                  <span className="theme-swatch theme-scope" data-theme={t.id} data-tone={t.tone} aria-hidden="true">
                    <i />
                    <i />
                  </span>
                  <span className="theme-name">{t.name}</span>
                  <svg className="theme-check" viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
                    <path d="M2.5 6.5 5 9l4.5-6" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
                  </svg>
                </button>
              ))}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
