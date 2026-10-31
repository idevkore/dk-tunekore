// The window's colour theme: which themes there are, the one in use (kept in
// localStorage, so it carries across launches), and the colours the
// Timeline's canvases draw with. The colours themselves are CSS tokens:
// tuneKore's on :root in App.css, the rest in themes.css.
import { useSyncExternalStore } from "react";

export type Tone = "dark" | "light";

export interface Theme {
  id: string;
  name: string;
  tone: Tone;
}

/** The default: the tuneKore look. */
export const DEFAULT_THEME = "tunekore";

export const THEMES: Theme[] = [
  { id: "tunekore", name: "tuneKore", tone: "dark" },
  { id: "dracula", name: "Dracula", tone: "dark" },
  { id: "nord", name: "Nord", tone: "dark" },
  { id: "gruvbox-dark", name: "Gruvbox Dark", tone: "dark" },
  { id: "tokyo-night", name: "Tokyo Night", tone: "dark" },
  { id: "solarized-light", name: "Solarized Light", tone: "light" },
  { id: "github-light", name: "GitHub Light", tone: "light" },
  { id: "gruvbox-light", name: "Gruvbox Light", tone: "light" },
];

const STORE = "tunekore.theme";

const find = (id: string | null) => THEMES.find((t) => t.id === id);

/** The theme saved last time, or the default. */
export function storedTheme(): Theme {
  try {
    return find(localStorage.getItem(STORE)) ?? find(DEFAULT_THEME)!;
  } catch {
    return find(DEFAULT_THEME)!;
  }
}

let current: Theme = find(DEFAULT_THEME)!;
const listeners = new Set<() => void>();

/**
 * The tokens worked out from a theme's core ones: [token, from, to, t], the
 * colour `t` of the way from `from` to `to`. tuneKore has its own exact
 * values in App.css; every other theme gets these. They are worked out here
 * rather than with CSS color-mix(), which older macOS WebKit can't draw.
 */
export const DERIVED: [string, string, string, number][] = [
  ["--btn-text", "--soft", "--text", 0.6],
  ["--panel-line", "--line", "--line-2", 0.2],
  ["--control-line", "--line-2", "--text", 0.06],
  ["--hover-line", "--line-2", "--text", 0.2],
  ["--select", "--card", "--violet", 0.11],
  ["--select-2", "--side", "--violet", 0.09],
  ["--hot-bg", "--card", "--lime", 0.12],
  ["--well", "--card-2", "--text", 0.04],
  ["--well-alt", "--card", "--card-2", 0.4],
  ["--pad-line", "--line", "--text", 0.03],
  ["--pad-hover", "--line-2", "--text", 0.1],
  ["--pad-loaded", "--card", "--violet", 0.06],
  ["--pad-empty", "--card", "--text", 0.22],
  ["--wave", "--violet", "--text", 0.22],
  ["--cell-line", "--line-2", "--card", 0.15],
  ["--cell-beat", "--line-2", "--text", 0.1],
  ["--cell-hover", "--line-2", "--text", 0.25],
  ["--accent-dim", "--card", "--violet", 0.35],
  ["--accent-hi", "--violet", "--text", 0.25],
  ["--accent-mute", "--card", "--violet", 0.45],
  ["--midi-on", "--card", "--violet", 0.5],
  ["--warn-line", "--card", "--warn", 0.5],
  ["--danger-line", "--card", "--danger", 0.65],
  ["--rec-peak", "--danger", "--text", 0.55],
  ["--arm", "--card-2", "--danger", 0.4],
];

/** See-through tokens: [token, colour, alpha]. */
const WASHES: [string, string, number][] = [
  ["--accent-wash", "--violet", 0.08],
  ["--lime-glow", "--lime", 0.5],
  ["--lime-fade", "--lime", 0.3],
];

/** Set the derived tokens on <html> for a theme already applied; tuneKore clears them. */
function derive(root: HTMLElement, theme: Theme) {
  for (const [name] of [...DERIVED, ...WASHES]) root.style.removeProperty(name);
  if (theme.id === DEFAULT_THEME) return;
  const css = getComputedStyle(root);
  const read = (name: string) => {
    const v = css.getPropertyValue(name).trim();
    return /^#[0-9a-f]{6}$/i.test(v) ? v : null;
  };
  const core = new Map<string, string>();
  for (const [, from, to] of DERIVED) for (const n of [from, to]) if (!core.has(n)) core.set(n, read(n) ?? "");
  // Stylesheets not in yet (the first paint): try again once they are.
  if ([...core.values()].some((v) => !v)) {
    window.addEventListener("load", () => current === theme && derive(root, theme), { once: true });
    return;
  }
  for (const [name, from, to, t] of DERIVED) root.style.setProperty(name, mix(core.get(from)!, core.get(to)!, t));
  for (const [name, colour, a] of WASHES) root.style.setProperty(name, alpha(read(colour) ?? "#000000", a));
}

/** Put a theme on the window, and remember it unless `remember` is false. */
export function applyTheme(id: string, remember = true) {
  const theme = find(id) ?? find(DEFAULT_THEME)!;
  const root = document.documentElement;
  root.dataset.theme = theme.id;
  root.dataset.tone = theme.tone;
  current = theme;
  cached = null;
  derive(root, theme);
  if (remember) {
    try {
      localStorage.setItem(STORE, theme.id);
    } catch {
      // private storage: the theme still applies for this run
    }
  }
  for (const f of listeners) f();
}

export function currentTheme(): Theme {
  return current;
}

/** Called after every change of theme; returns the unsubscribe. */
export function onTheme(f: () => void): () => void {
  listeners.add(f);
  return () => listeners.delete(f);
}

/** The theme in use, re-rendering on a change. */
export function useTheme(): Theme {
  return useSyncExternalStore(onTheme, currentTheme);
}

// ---- Canvas colours ----

/** The colours the Timeline's canvases draw with, read from the tokens. */
export interface Palette {
  tone: Tone;
  ground: string;
  deep: string;
  side: string;
  line: string;
  text: string;
  dim: string;
  faint: string;
  accent: string;
  accentSoft: string;
  highlight: string;
  warn: string;
  laneB: string;
  clip: string;
  clipSel: string;
  /** For the grid and outlines, drawn with alpha (see `alpha`). */
  ink: string;
  /** How much of a track's colour tints its clips, 0..1. */
  clipTint: number;
  /** How far a waveform is shaded toward the text, 0..1. */
  waveShade: number;
}

/** tuneKore's values, for a token that is missing or not hex. */
const FALLBACK: Palette = {
  tone: "dark",
  ground: "#121217",
  deep: "#0c0c10",
  side: "#16161c",
  line: "#24242d",
  text: "#f7f7f8",
  dim: "#a6a6b3",
  faint: "#8a8a98",
  accent: "#8051ff",
  accentSoft: "#c9b8ff",
  highlight: "#b7ff00",
  warn: "#ffb36b",
  laneB: "#141419",
  clip: "#1d1b27",
  clipSel: "#2c2546",
  ink: "#ffffff",
  clipTint: 0,
  waveShade: 0,
};

let cached: Palette | null = null;

export function palette(): Palette {
  if (cached) return cached;
  const css = getComputedStyle(document.documentElement);
  const hex = (name: string, fallback: string) => {
    const v = css.getPropertyValue(name).trim();
    return /^#[0-9a-f]{6}$/i.test(v) ? v.toLowerCase() : fallback;
  };
  const num = (name: string, fallback: number) => {
    const v = parseFloat(css.getPropertyValue(name));
    return Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : fallback;
  };
  const f = FALLBACK;
  cached = {
    tone: current.tone,
    ground: hex("--ground", f.ground),
    deep: hex("--deep", f.deep),
    side: hex("--side", f.side),
    line: hex("--line", f.line),
    text: hex("--text", f.text),
    dim: hex("--dim", f.dim),
    faint: hex("--faint", f.faint),
    accent: hex("--violet", f.accent),
    accentSoft: hex("--violet-soft", f.accentSoft),
    highlight: hex("--lime", f.highlight),
    warn: hex("--warn", f.warn),
    laneB: hex("--lane-b", f.laneB),
    clip: hex("--clip", f.clip),
    clipSel: hex("--clip-sel", f.clipSel),
    ink: hex("--grid-ink", f.ink),
    clipTint: num("--clip-tint", f.clipTint),
    waveShade: num("--wave-shade", f.waveShade),
  };
  return cached;
}

const rgb = (hex: string): [number, number, number] => {
  const n = parseInt(hex.slice(1, 7), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
};

/** A hex colour with alpha, as rgba(). */
export function alpha(hex: string, a: number): string {
  const [r, g, b] = rgb(hex);
  return `rgba(${r},${g},${b},${a})`;
}

/** `a` moved `t` (0..1) of the way to `b`, as hex. */
export function mix(a: string, b: string, t: number): string {
  if (t <= 0) return a;
  const x = rgb(a);
  const y = rgb(b);
  const c = x.map((v, i) => Math.round(v + (y[i] - v) * Math.min(1, t)));
  return `#${c.map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}
