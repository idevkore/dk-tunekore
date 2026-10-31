// The Timeline view (idevkore/dk-tunekore#36): tracks and audio clips on the
// transport's beat grid, the same clock the Sampler plays on. The Sampler's
// track comes first, with its mix; every other track is an audio track.
//
// Layout: the top bar (the view switch, play, the playhead's position, the
// tempo, the loop, the grid, zoom), the Library on the left (the Sampler's,
// shared), then the ruler over the lanes, with the track headers beside
// them. The lanes and the ruler are canvases; the playhead is a DOM line
// over both, moved every frame from the transport's ticks.
//
// It stays mounted while the Sampler shows (App.tsx), hidden, so its
// Library, selection and view carry on; `active` says whether it is on screen.
//
// Keys while it shows: Space plays and stops (taken here, so the Sampler's
// own Space handler doesn't also toggle; a focused button or checkbox other
// than the view switch takes its own Space), ⌘Z and ⇧⌘Z undo and redo the
// Timeline's edits, ⌫ deletes the selected clips, ⌘D duplicates them, ⌘E
// splits at the playhead, ⌘L loops the selection, ⌘A selects every clip,
// = and - zoom, Home goes back to the start. A focused track header takes
// F2 or Enter to rename it.
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import logo from "../../brand/tunekore-logo-color.svg";
import mark from "../../brand/tunekore-mark.svg";
import StatusBar, { type Message } from "../StatusBar";
import Library from "../sampler/Library";
import type { LibraryItem } from "../sampler/api";
import { onMeter } from "../engine";
import * as api from "./api";
import { AUDIO_EXTENSIONS, subscribe, type Edit, type Snap, type Tick, type TimelineView } from "./api";
import { barBeats, clipEnd, gridStep, position, secondsPerBeat, snapBeat, toInput } from "./edits";
import { beatToX, type Ghost, type Lane } from "./draw";
import Lanes from "./Lanes";
import Ruler from "./Ruler";
import TrackHeader from "./TrackHeader";
import ClipInspector from "./ClipInspector";
import Dialog from "../sampler/Dialog";
import DeviceChain from "../plugins/DeviceChain";
import PluginPicker from "../plugins/PluginPicker";
import * as plugins from "../plugins/api";
import type { CatalogEntry, PluginsView } from "../plugins/api";
import { retryFailed } from "./peaks";
import "./timeline.css";

const mod = navigator.platform.toLowerCase().includes("mac") ? "⌘" : "Ctrl+";
const HEADER_W = 236;
/** The area under the last track that takes drops for a new track. */
const NEW_TRACK_ZONE = 64;
const MIN_ZOOM = 1;
const MAX_ZOOM = 800;
const DRAG_START_PX = 6;
const SNAPS: { id: Snap; label: string }[] = [
  { id: "bar", label: "Bar" },
  { id: "beat", label: "Beat" },
  { id: "eighth", label: "1/8" },
  { id: "sixteenth", label: "1/16" },
  { id: "off", label: "Off" },
];

type Props = {
  /** Whether the Timeline is the view on screen. */
  active: boolean;
  switcher: ReactNode;
  message: Message | null;
  onMessage: (m: Message | null) => void;
};

/** A drop on its way from the Library or Finder: what and where. */
type Drop = { paths: string[]; lane: number; track: number | null; beat: number };

const isAudio = (p: string) => AUDIO_EXTENSIONS.includes(p.split(".").pop()?.toLowerCase() ?? "");
const baseName = (p: string) => (p.split(/[\\/]/).pop() ?? p).replace(/\.\w+$/, "");
const typing = (t: EventTarget | null) => {
  const el = t as HTMLElement | null;
  if (!el?.tagName) return false;
  if (el.tagName === "TEXTAREA" || el.tagName === "SELECT") return true;
  return el.tagName === "INPUT" && !["range", "checkbox"].includes((el as HTMLInputElement).type);
};

/** Peak 0..1 to a 0..1 bar position on a -60..0 dB scale. */
const meterPosition = (peak: number) => (peak <= 0 ? 0 : Math.min(1, Math.max(0, (20 * Math.log10(peak) + 60) / 60)));
/** The same signature keeps its identity, so the canvases don't redraw on every tick. */
const sameSig = (next: [number, number]) => (s: [number, number]) => (s[0] === next[0] && s[1] === next[1] ? s : next);

export default function Timeline({ active, switcher, message, onMessage }: Props) {
  const [view, setView] = useState<TimelineView | null>(null);
  const [zoom, setZoom] = useState(24);
  const [scroll, setScroll] = useState(0);
  const [scrollTop, setScrollTop] = useState(0);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [track, setTrack] = useState<number | null>(null);
  const [heights, setHeights] = useState<Record<number, number>>({});
  const [playing, setPlaying] = useState(false);
  const [tempo, setTempo] = useState(120);
  const [tempoText, setTempoText] = useState("120");
  const [signature, setSignature] = useState<[number, number]>([4, 4]);
  const [follow, setFollow] = useState(true);
  const [drop, setDrop] = useState<Drop | null>(null);
  const [libDrag, setLibDrag] = useState<{ item: LibraryItem; x0: number; y0: number; moving: boolean } | null>(null);
  const [libraryFolder, setLibraryFolder] = useState<string | null>(null);
  const [devices, setDevices] = useState<PluginsView | null>(null);
  const [pickInstrument, setPickInstrument] = useState(false);
  const [removing, setRemoving] = useState<{ id: number; name: string } | null>(null);

  const scrollEl = useRef<HTMLDivElement>(null);
  const hscroll = useRef<HTMLDivElement>(null);
  const lanesBox = useRef<HTMLDivElement>(null);
  const laneHead = useRef<HTMLDivElement>(null);
  const rulerHead = useRef<HTMLDivElement>(null);
  const posText = useRef<HTMLSpanElement>(null);
  // Meters move by writing to their elements: no render 30 times a second.
  const meters = useRef(new Map<number, HTMLDivElement>());
  const masterMeter = useRef<HTMLDivElement>(null);
  const masterLevel = useRef(0);
  const activeRef = useRef(active);
  activeRef.current = active;
  const tick = useRef<Tick & { at: number }>({ playing: false, beat: 0, tempo: 120, signature: [4, 4], levels: [], at: 0 });
  const live = useRef({ zoom, scroll, size, follow, view });
  live.current = { zoom, scroll, size, follow, view };

  const say = useCallback((text: string, error = false) => onMessage({ text, error }), [onMessage]);
  const fail = useCallback((e: unknown) => say(String(e), true), [say]);
  const take = useCallback((v: TimelineView) => {
    setView(v);
    setTempo(v.tempo);
    setSignature(sameSig(v.signature));
    retryFailed();
  }, []);
  /** The view's own settings (zoom, scroll, grid): at first, and after a project opens. */
  const adopt = useCallback(
    (v: TimelineView) => {
      take(v);
      setZoom(v.zoom);
      setScroll(v.scroll);
      setSelected(new Set());
    },
    [take],
  );

  // ---- State, ticks and the meter ----

  useEffect(() => {
    api.state().then(adopt).catch(fail);
    const offs = [
      subscribe(api.onState(take)),
      subscribe(
        api.onTick((t) => {
          tick.current = { ...t, at: performance.now() };
          setPlaying(t.playing);
          setTempo(t.tempo);
          setSignature(sameSig(t.signature));
          for (const [id, peak] of t.levels) {
            const el = meters.current.get(id);
            if (el) el.style.transform = `scaleX(${meterPosition(peak)})`;
          }
          if (!t.playing) placeHead.current();
        }),
      ),
      subscribe(
        onMeter((peak) => {
          masterLevel.current = Math.max(peak, masterLevel.current * 0.8);
          if (masterMeter.current) masterMeter.current.style.transform = `scaleX(${meterPosition(masterLevel.current)})`;
        }),
      ),
    ];
    try {
      offs.push(subscribe(listen<TimelineView>("timeline:opened", (e) => adopt(e.payload))));
    } catch {
      // not in Tauri (the dev mock)
    }
    // The plugins on the tracks, and the ones a project named that didn't load.
    plugins.state().then(setDevices).catch(() => {});
    offs.push(subscribe(plugins.onState(setDevices)));
    offs.push(subscribe(plugins.onMissing((names) => fail(`These plugins didn't load: ${names.join(", ")}. Their sound is kept; Reload them once they're installed.`))));
    return () => offs.forEach((off) => off());
  }, [take, adopt, fail]);

  useEffect(() => setTempoText(String(Math.round(tempo * 100) / 100)), [tempo]);

  // A selection holds only clips that still exist.
  useEffect(() => {
    if (!view) return;
    const ids = new Set(view.tracks.flatMap((t) => t.clips.map((c) => c.id)));
    setSelected((s) => ([...s].every((id) => ids.has(id)) ? s : new Set([...s].filter((id) => ids.has(id)))));
  }, [view]);

  // A MIDI controller's keys play the selected track's instrument while the
  // Timeline shows; otherwise (the Sampler, or another kind of track) the pads.
  const focusTrack = active ? view?.tracks.find((t) => t.id === track && t.kind === "instrument")?.id ?? null : null;
  const hasInstrument = focusTrack !== null && (devices?.devices ?? []).some((d) => d.track === focusTrack && d.role === "instrument" && d.status === "ok");
  useEffect(() => {
    plugins.focus(hasInstrument ? focusTrack : null).catch(() => {});
  }, [focusTrack, hasInstrument]);

  // The view's settings go back to the Rust side (saved with the project) once they settle.
  const snap = view?.snap ?? "beat";
  const library = view?.library ?? true;
  useEffect(() => {
    if (!view) return;
    const t = setTimeout(() => void api.setView({ zoom, scroll }).catch(() => {}), 400);
    return () => clearTimeout(t);
  }, [zoom, scroll, view === null]);

  // ---- Layout ----

  // Coming back on screen: the scroll positions again, in case the webview
  // dropped them while the view was hidden.
  useEffect(() => {
    if (!active) return;
    if (scrollEl.current) scrollEl.current.scrollTop = scrollTop;
    if (hscroll.current) hscroll.current.scrollLeft = scroll * zoom;
  }, [active]);

  useEffect(() => {
    const el = scrollEl.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setSize({ w: Math.max(0, el.clientWidth - HEADER_W), h: el.clientHeight }));
    ro.observe(el);
    return () => ro.disconnect();
  }, [view === null]);

  const lanes: Lane[] = useMemo(() => {
    let top = 0;
    return (view?.tracks ?? []).map((t) => {
      const height = heights[t.id] ?? t.height;
      const lane = { track: t, top, height };
      top += height;
      return lane;
    });
  }, [view, heights]);
  const end = lanes.length ? lanes[lanes.length - 1].top + lanes[lanes.length - 1].height : 0;
  const sources = useMemo(() => new Map((view?.sources ?? []).map((s) => [s.path, s])), [view]);
  const bar = barBeats(signature);
  const step = gridStep(snap, bar);
  const loop = useMemo(() => ({ on: view?.loopOn ?? false, start: view?.loopStart ?? 0, end: view?.loopEnd ?? 16 }), [view]);
  const allClips = useMemo(() => lanes.flatMap((l) => l.track.clips.map((c) => ({ clip: c, track: l.track.id }))), [lanes]);
  const songEnd = Math.max(loop.end, ...allClips.map(({ clip }) => clipEnd(clip, tempo)), 0);
  const contentBeats = songEnd + 32 * bar;

  // The horizontal scrollbar follows the scroll, and moves it.
  useEffect(() => {
    const el = hscroll.current;
    if (el && Math.abs(el.scrollLeft - scroll * zoom) > 1) el.scrollLeft = scroll * zoom;
  }, [scroll, zoom, contentBeats]);

  const zoomBy = useCallback((factor: number, atX: number) => {
    const { zoom, scroll } = live.current;
    const next = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom * factor));
    const beat = scroll + atX / zoom;
    setZoom(next);
    setScroll(Math.max(0, beat - atX / next));
  }, []);
  const pan = useCallback((beats: number) => setScroll((s) => Math.max(0, s + beats)), []);

  // ---- The playhead, every frame ----

  const playhead = useCallback(() => {
    const t = tick.current;
    const v = live.current.view;
    if (!t.playing) return t.beat;
    let beat = t.beat + ((performance.now() - t.at) / 1000) * (t.tempo / 60);
    if (v?.loopOn && t.beat < v.loopEnd && beat >= v.loopEnd && v.loopEnd > v.loopStart) {
      beat = v.loopStart + ((beat - v.loopEnd) % (v.loopEnd - v.loopStart));
    }
    return beat;
  }, []);

  /** Puts the playhead line and the position where the transport is now. */
  const placeHead = useRef(() => {});
  placeHead.current = () => {
    const { zoom, scroll, size, follow } = live.current;
    const beat = playhead();
    const x = beatToX(beat, scroll, zoom);
    for (const el of [laneHead.current, rulerHead.current]) {
      if (!el) continue;
      el.style.transform = `translateX(${Math.round(x)}px)`;
      el.style.visibility = x >= 0 && x <= size.w ? "visible" : "hidden";
    }
    if (posText.current) posText.current.textContent = position(beat, tick.current.signature);
    // Follow: a page on when the playhead runs off the right.
    if (follow && tick.current.playing && size.w > 0 && (x > size.w - 24 || x < 0)) setScroll(Math.max(0, beat - (0.1 * size.w) / zoom));
  };

  // Every frame while playing and on screen; once on each change while stopped.
  useEffect(() => {
    if (!active) return;
    placeHead.current();
    if (!playing) return;
    let raf = 0;
    const frame = () => {
      placeHead.current();
      raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
  }, [active, playing, zoom, scroll, size, view]);

  // ---- Edits ----

  const send = useCallback(
    (e: Edit) =>
      api.edit(e).then(
        (v) => {
          take(v);
          return v;
        },
        (err) => {
          fail(err);
          throw err;
        },
      ),
    [take, fail],
  );

  const addFiles = useCallback(
    (paths: string[], trackId: number | null, beat: number) => {
      const files = paths.filter(isAudio);
      if (files.length === 0) return;
      say(files.length === 1 ? `Adding ${baseName(files[0])}…` : `Adding ${files.length} files…`);
      api
        .addFiles(files, trackId, beat)
        .then((v) => {
          take(v);
          onMessage(null);
        })
        .catch(fail);
    },
    [take, fail, say, onMessage],
  );

  const togglePlay = useCallback(() => {
    api.play(!tick.current.playing).catch(fail);
  }, [fail]);

  const seek = useCallback((beat: number) => {
    tick.current = { ...tick.current, beat, at: performance.now() };
    placeHead.current();
    api.seek(beat).catch(fail);
  }, [fail]);

  const selectedClips = useCallback(() => allClips.filter(({ clip }) => selected.has(clip.id)), [allClips, selected]);

  const deleteSelected = useCallback(() => {
    if (selected.size) void send({ key: "delete", clips: [], delete: [...selected] }).catch(() => {});
  }, [selected, send]);

  const duplicate = useCallback(() => {
    const picked = selectedClips();
    if (!picked.length) return;
    const from = Math.min(...picked.map(({ clip }) => clip.start));
    const to = Math.max(...picked.map(({ clip }) => clipEnd(clip, tempo)));
    const before = new Set(allClips.map(({ clip }) => clip.id));
    void send({ key: "duplicate", clips: picked.map(({ clip, track }) => ({ ...toInput({ ...clip, start: clip.start + (to - from) }, track), id: null })), delete: [] })
      .then((v) => setSelected(new Set(v.tracks.flatMap((t) => t.clips.map((c) => c.id)).filter((id) => !before.has(id)))))
      .catch(() => {});
  }, [selectedClips, allClips, tempo, send]);

  const split = useCallback(() => {
    api.split([...selected], playhead()).then(take).catch(fail);
  }, [selected, playhead, take, fail]);

  const setLoop = useCallback((l: { on: boolean; start: number; end: number }) => api.setLoop(l.on, l.start, l.end).then(take).catch(fail), [take, fail]);

  const loopSelection = useCallback(() => {
    const picked = selectedClips();
    if (!picked.length) return;
    void setLoop({ on: true, start: Math.min(...picked.map(({ clip }) => clip.start)), end: Math.max(...picked.map(({ clip }) => clipEnd(clip, tempo))) });
  }, [selectedClips, tempo, setLoop]);

  const undo = useCallback(() => api.undo().then(take).catch(fail), [take, fail]);
  const redo = useCallback(() => api.redo().then(take).catch(fail), [take, fail]);
  const addTrack = useCallback(() => api.addTrack().then(take).catch(fail), [take, fail]);
  /** A new instrument track with `p` on it, selected. */
  const addInstrument = useCallback(
    (p: CatalogEntry) => {
      setPickInstrument(false);
      say(`Loading ${p.name}…`);
      const before = new Set((devices?.devices ?? []).map((d) => d.id));
      // Loading takes the UI's thread for a moment (Kontakt can take seconds): show the message first.
      plugins.afterPaint(() => {
        plugins
          .add(p, null)
          .then((v) => {
            setDevices(v);
            const added = v.devices.find((d) => !before.has(d.id));
            if (added) setTrack(added.track);
            say(`${p.name} is on a new instrument track`);
          })
          .catch((e) => fail(`${p.name} didn't load: ${e}`));
      });
    },
    [devices, say, fail],
  );
  const removeInstrumentTrack = useCallback(
    (id: number) => {
      setRemoving(null);
      plugins
        .removeTrack(id)
        .then((v) => {
          setDevices(v);
          if (track === id) setTrack(null);
        })
        .catch(fail);
    },
    [track, fail],
  );
  const setSnap = (s: Snap) => api.setView({ snap: s }).then(take).catch(fail);
  const setLibrary = (on: boolean) => api.setView({ library: on }).then(take).catch(fail);

  const commitTempo = () => {
    const t = Number(tempoText);
    if (Number.isFinite(t) && t >= 40 && t <= 300) api.setTempo(t).catch(fail);
    else setTempoText(String(tempo));
  };
  const nudgeTempo = (d: number) => api.setTempo(Math.min(300, Math.max(40, Math.round(tempo + d)))).catch(fail);

  // ---- Keyboard ----

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!activeRef.current || document.querySelector("dialog[open]") || typing(e.target)) return;
      const k = e.key.toLowerCase();
      const cmd = e.metaKey || e.ctrlKey;
      const el = e.target as HTMLElement | null;
      if (e.key === " " && !cmd && !e.altKey) {
        // The Sampler stays mounted with a Space handler of its own: while the
        // Timeline shows, Space is the Timeline's, so it never sees it.
        e.stopImmediatePropagation();
        // A focused button or checkbox takes its own Space, except the view
        // switch's, which keep focus after a switch.
        const control = el?.closest?.("button, [role=button], input[type=checkbox], input[type=radio]");
        if (control && !control.closest(".view-switch")) return;
        e.preventDefault();
        if (!e.repeat) togglePlay();
        return;
      }
      // A focused slider keeps its own keys (Home, End, the arrows).
      if (el?.tagName === "INPUT") return;
      const run = (f: () => void) => {
        e.preventDefault();
        e.stopImmediatePropagation();
        f();
      };
      if (cmd && !e.altKey) {
        if (k === "z") return run(() => void (e.shiftKey ? redo() : undo()));
        if (e.shiftKey) return;
        if (k === "d") return run(duplicate);
        if (k === "e") return run(split);
        if (k === "l") return run(loopSelection);
        if (k === "a") return run(() => setSelected(new Set(allClips.map(({ clip }) => clip.id))));
        return;
      }
      if (e.altKey) return;
      if (e.key === "Backspace" || e.key === "Delete") return run(deleteSelected);
      // Escape goes on to the Library too (it stops a preview there).
      if (e.key === "Escape" && selected.size) return setSelected(new Set());
      if (e.key === "=" || e.key === "+") return run(() => zoomBy(1.25, live.current.size.w / 2));
      if (e.key === "-") return run(() => zoomBy(0.8, live.current.size.w / 2));
      if (e.key === "Home") return run(() => {
        seek(0);
        setScroll(0);
      });
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [togglePlay, undo, redo, duplicate, split, loopSelection, deleteSelected, zoomBy, seek, allClips, selected]);

  // ---- Drops: from the Library and from Finder ----

  /** Where a drop at window point (x, y) lands: a lane and a beat, or a new track below the last. */
  const dropAt = useCallback(
    (paths: string[], x: number, y: number): Drop | null => {
      const box = lanesBox.current?.getBoundingClientRect();
      const { zoom, scroll } = live.current;
      if (!box || x < box.left - HEADER_W || x > box.right || y < box.top || y > box.bottom) return null;
      const at = y - box.top + scrollTop;
      const i = lanes.findIndex((l) => at >= l.top && at < l.top + l.height);
      const onAudio = i >= 0 && lanes[i].track.kind === "audio";
      const beat = Math.max(0, snapBeat(scroll + Math.max(0, x - box.left) / zoom, step));
      return { paths, lane: onAudio ? i : lanes.length, track: onAudio ? lanes[i].track.id : null, beat };
    },
    [lanes, scrollTop, step],
  );

  const ghost: Ghost | null = useMemo(() => {
    if (!drop) return null;
    const src = sources.get(drop.paths[0]);
    const length = src && !src.missing ? src.seconds / secondsPerBeat(tempo) : bar;
    const more = drop.paths.length > 1 ? ` +${drop.paths.length - 1}` : "";
    return { lane: drop.lane, start: drop.beat, length, label: baseName(drop.paths[0]) + more };
  }, [drop, sources, tempo, bar]);

  // Dragging from the Library: the Library starts it, the window tracks the pointer.
  useEffect(() => {
    if (!libDrag) return;
    const move = (e: PointerEvent) => {
      const moving = libDrag.moving || Math.hypot(e.clientX - libDrag.x0, e.clientY - libDrag.y0) > DRAG_START_PX;
      if (moving !== libDrag.moving) setLibDrag({ ...libDrag, moving });
      setDrop(moving ? dropAt([libDrag.item.path], e.clientX, e.clientY) : null);
    };
    const up = (e: PointerEvent) => {
      const d = libDrag.moving ? dropAt([libDrag.item.path], e.clientX, e.clientY) : null;
      if (d) addFiles(d.paths, d.track, d.beat);
      setLibDrag(null);
      setDrop(null);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
  }, [libDrag, dropAt, addFiles]);

  // Files from Finder. A project file is the Sampler's to open; a folder becomes the Library.
  const finder = useRef<string[]>([]);
  const dropAtRef = useRef(dropAt);
  dropAtRef.current = dropAt;
  const addRef = useRef(addFiles);
  addRef.current = addFiles;
  useEffect(() => {
    let off: (() => void) | undefined;
    let gone = false;
    let webview;
    try {
      webview = getCurrentWebview();
    } catch {
      return; // not in Tauri (the dev mock)
    }
    const dpr = () => window.devicePixelRatio || 1;
    webview
      .onDragDropEvent((e) => {
        // While the Sampler shows, its drops are its own.
        if (!activeRef.current) return;
        const p = e.payload;
        if (p.type === "enter") finder.current = p.paths.filter(isAudio);
        if (p.type === "leave") {
          finder.current = [];
          setDrop(null);
          return;
        }
        const files = p.type === "drop" ? p.paths.filter(isAudio) : finder.current;
        if (p.type === "drop" && files.length === 0 && p.paths.length === 1 && !p.paths[0].toLowerCase().endsWith(".tunek")) {
          setLibraryFolder(p.paths[0]);
        }
        if (files.length === 0) return;
        const d = dropAtRef.current(files, p.position.x / dpr(), p.position.y / dpr());
        if (p.type === "drop") {
          setDrop(null);
          finder.current = [];
          // Anywhere else in the window: a new track at the start.
          addRef.current(files, d?.track ?? null, d?.beat ?? 0);
        } else setDrop(d);
      })
      .then((u) => (gone ? u() : (off = u)))
      .catch(() => {});
    return () => {
      gone = true;
      off?.();
    };
  }, []);

  if (!view) {
    return (
      <div className="app timeline" hidden={!active}>
        <header className="bar">
          <img src={logo} alt="tuneKore" className="bar-logo wide-only" />
          <img src={mark} alt="tuneKore" className="bar-mark narrow-only" />
          {switcher}
        </header>
        <main className="tl-loading dim">Loading the Timeline…</main>
        <StatusBar message={message} onDismiss={() => onMessage(null)}>
          <kbd>{mod}1</kbd> Sampler
        </StatusBar>
      </div>
    );
  }

  const one = selected.size === 1 ? allClips.find(({ clip }) => selected.has(clip.id)) : undefined;
  const selectedTrack = view.tracks.find((t) => t.id === track) ?? null;
  const contentPx = contentBeats * zoom + size.w;
  const empty = allClips.length === 0;

  return (
    <div className={`app timeline${libDrag?.moving ? " dragging" : ""}`} hidden={!active}>
      <header className="bar">
        <img src={logo} alt="tuneKore" className="bar-logo wide-only" />
        <img src={mark} alt="tuneKore" className="bar-mark narrow-only" />
        {switcher}
        <div className="transport">
          <button className={`play${playing ? " on" : ""}`} onClick={togglePlay} aria-label={playing ? "Stop" : "Play"} title="Play or stop (Space)">
            {playing ? (
              <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
                <rect x="2" y="2" width="10" height="10" rx="1.5" fill="currentColor" />
              </svg>
            ) : (
              <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
                <path d="M3 1.8v10.4L12 7z" fill="currentColor" />
              </svg>
            )}
          </button>
          <span className="tl-pos mono" ref={posText} title="The playhead: bar, beat, sixteenth">
            1.1.1
          </span>
          <div className="tempo">
            <button className="nudge" onClick={() => void nudgeTempo(-1)} aria-label="Slower" title="Slower">
              −
            </button>
            <input
              className="mono"
              value={tempoText}
              inputMode="decimal"
              aria-label="Tempo in BPM"
              onChange={(e) => setTempoText(e.target.value)}
              onBlur={commitTempo}
              onKeyDown={(e) => e.key === "Enter" && (e.currentTarget as HTMLInputElement).blur()}
            />
            <button className="nudge" onClick={() => void nudgeTempo(1)} aria-label="Faster" title="Faster">
              +
            </button>
            <span className="unit">BPM</span>
          </div>
          <span className="tl-sig mono" title="Time signature">
            {signature[0]}/{signature[1]}
          </span>
        </div>
        <button className={`tl-tool${loop.on ? " on" : ""}`} aria-pressed={loop.on} aria-label="Loop" onClick={() => void setLoop({ ...loop, on: !loop.on })} title={`Loop the range on the ruler (${mod}L loops the selected clips)`}>
          <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
            <path d="M3 9.5V7a3 3 0 0 1 3-3h7M11 2l2 2-2 2M13 6.5V9a3 3 0 0 1-3 3H3M5 14l-2-2 2-2" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
          <span className="tl-tool-label">Loop</span>
        </button>
        <button className={`tl-tool${follow ? " on" : ""}`} aria-pressed={follow} aria-label="Follow the playhead" onClick={() => setFollow(!follow)} title="Follow the playhead while playing">
          <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
            <path d="M2 8h9M8 4.5 11.5 8 8 11.5M14 3v10" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
          <span className="tl-tool-label">Follow</span>
        </button>
        <label className="len tl-grid" title={`What clips, the loop and the playhead snap to (hold ${mod} while dragging to ignore it)`}>
          <span>Grid</span>
          <select value={snap} onChange={(e) => void setSnap(e.target.value as Snap)}>
            {SNAPS.map((s) => (
              <option key={s.id} value={s.id}>
                {s.label}
              </option>
            ))}
          </select>
        </label>
        <div className="group-btns" role="group" aria-label="Zoom">
          <button className="btn icon small" onClick={() => zoomBy(0.8, size.w / 2)} aria-label="Zoom out" title="Zoom out (-)">
            −
          </button>
          <button className="btn icon small" onClick={() => zoomBy(1.25, size.w / 2)} aria-label="Zoom in" title="Zoom in (=)">
            +
          </button>
        </div>
        <button className="btn small" onClick={split} disabled={empty} title={`Split the selected clips, or every clip, at the playhead (${mod}E)`}>
          Split
        </button>
        <button className="btn small" onClick={() => void addTrack()} title="Add an empty audio track, for clips">
          + Audio
        </button>
        <button className="btn small" onClick={() => setPickInstrument(true)} title="Add an instrument track with a plugin instrument on it">
          + Instrument
        </button>
        <span className="grow" />
        <div className="group-btns">
          <button className="btn icon small" onClick={() => void undo()} disabled={!view.canUndo} aria-label="Undo" title={`Undo (${mod}Z)`}>
            ↶
          </button>
          <button className="btn icon small" onClick={() => void redo()} disabled={!view.canRedo} aria-label="Redo" title={`Redo (⇧${mod}Z)`}>
            ↷
          </button>
        </div>
        <button className={`tl-tool${library ? " on" : ""}`} aria-pressed={library} aria-label="Library" onClick={() => void setLibrary(!library)} title="Show the Library">
          <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
            <path d="M2.5 2.5h3v11h-3zM7 2.5h3v11H7zM11.5 3l2.5.6-2.4 10.2-2.5-.6z" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinejoin="round" />
          </svg>
          <span className="tl-tool-label">Library</span>
        </button>
        <div className="meter" title="Output level" aria-hidden="true">
          <div className="meter-fill" ref={masterMeter} style={{ transform: "scaleX(0)" }} />
        </div>
      </header>
      <div className="body tl-body">
        {library && (
          <Library
            selectedPad={0}
            onLoad={(item) => {
              const t = view.tracks.find((x) => x.id === track && x.kind === "audio");
              addFiles([item.path], t ? t.id : null, playhead());
            }}
            onDragStart={(item, x, y) => setLibDrag({ item, x0: x, y0: y, moving: false })}
            onError={fail}
            refresh={0}
            folder={libraryFolder}
            onFolder={setLibraryFolder}
          />
        )}
        <div className="tl-main">
          <div className="tl-ruler-row">
            <div className="tl-corner" style={{ width: HEADER_W }}>
              <span className="dim">{lanes.length} tracks</span>
            </div>
            <div className="tl-ruler-box">
              <Ruler width={size.w} scroll={scroll} zoom={zoom} signature={signature} loop={loop} step={step} onLoop={(l) => void setLoop(l)} onSeek={seek} onZoom={zoomBy} onPan={pan} />
              <div className="tl-playhead ruler" ref={rulerHead} aria-hidden="true" />
            </div>
          </div>
          <div className="tl-scroll" ref={scrollEl} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
            <div className="tl-rows" style={{ height: end + NEW_TRACK_ZONE }}>
              <div className="tl-heads" style={{ width: HEADER_W }}>
                {lanes.map((l) => (
                  <TrackHeader
                    key={l.track.id}
                    track={l.track}
                    height={l.height}
                    selected={track === l.track.id}
                    meter={(el) => {
                      if (el) meters.current.set(l.track.id, el);
                      else meters.current.delete(l.track.id);
                    }}
                    onSelect={() => setTrack(l.track.id)}
                    onMix={(mix) => api.setTrack(l.track.id, { mix }).then(take).catch(fail)}
                    onRename={(name) => api.setTrack(l.track.id, { name }).then(take).catch(fail)}
                    onColour={(colour) => api.setTrack(l.track.id, { colour }).then(take).catch(fail)}
                    onRemove={() => (l.track.kind === "instrument" ? setRemoving({ id: l.track.id, name: l.track.name }) : api.removeTrack(l.track.id).then(take).catch(fail))}
                    onHeight={(h, done) => {
                      const height = Math.round(Math.min(320, Math.max(28, h)));
                      setHeights((x) => ({ ...x, [l.track.id]: height }));
                      if (done)
                        api
                          .setTrack(l.track.id, { height })
                          .then((v) => {
                            take(v);
                            setHeights((x) => {
                              const { [l.track.id]: _, ...rest } = x;
                              return rest;
                            });
                          })
                          .catch(fail);
                    }}
                  />
                ))}
                <button className={`tl-add-track${drop && drop.track === null ? " over" : ""}`} style={{ height: NEW_TRACK_ZONE }} onClick={() => void addTrack()}>
                  + Audio track <span className="dim">or drop samples here</span>
                </button>
              </div>
              <div className="tl-lane-col">
                <div className="tl-sticky" ref={lanesBox} style={{ height: size.h }}>
                  <Lanes
                    lanes={lanes}
                    end={end}
                    width={size.w}
                    height={size.h}
                    scroll={scroll}
                    zoom={zoom}
                    scrollTop={scrollTop}
                    tempo={tempo}
                    signature={signature}
                    sources={sources}
                    selected={selected}
                    loop={loop}
                    step={step}
                    ghost={ghost}
                    onSelect={(ids) => setSelected(new Set(ids))}
                    onTrack={setTrack}
                    onSeek={seek}
                    onEdit={send}
                    onZoom={zoomBy}
                    onPan={pan}
                  />
                  <div className="tl-playhead" ref={laneHead} aria-hidden="true" />
                  {empty && !drop && (
                    <div className="tl-empty" style={{ top: Math.max(8, end - scrollTop + 10) }}>
                      <strong>Drag samples here</strong>
                      <span>From the Library or from Finder (stems the Analyzer exported, too). Each one dropped below the tracks gets a track of its own, lined up; dropped on a track, they play one after another.</span>
                    </div>
                  )}
                </div>
              </div>
            </div>
          </div>
          <div className="tl-hscroll" ref={hscroll} style={{ marginLeft: HEADER_W }} onScroll={(e) => setScroll(e.currentTarget.scrollLeft / zoom)} aria-hidden="true">
            <div style={{ width: contentPx, height: 1 }} />
          </div>
          {one ? (
            <ClipInspector
              clip={one.clip}
              tempo={tempo}
              signature={signature}
              missing={sources.get(one.clip.path)?.missing ?? true}
              onChange={(clip, key) => void send({ key, clips: [toInput(clip, one.track)], delete: [] }).catch(() => {})}
              onDelete={deleteSelected}
            />
          ) : (
            selectedTrack && <DeviceChain track={selectedTrack} plugins={devices} onPlugins={setDevices} onMessage={say} />
          )}
        </div>
      </div>
      {pickInstrument && <PluginPicker kind="instrument" title="New instrument track" onPick={addInstrument} onClose={() => setPickInstrument(false)} />}
      {removing && (
        <Dialog title={`Delete ${removing.name}?`} onClose={() => setRemoving(null)}>
          <p>The track goes with its instrument and effects, and their sound. This can't be undone yet.</p>
          <div className="dialog-foot">
            <button className="btn small" onClick={() => setRemoving(null)}>
              Keep it
            </button>
            <button className="btn small danger" onClick={() => removeInstrumentTrack(removing.id)}>
              Delete
            </button>
          </div>
        </Dialog>
      )}
      <StatusBar message={message} onDismiss={() => onMessage(null)}>
        <kbd>Space</kbd> play · drag samples onto tracks · <kbd>⌥</kbd>-drag copies · <kbd>{mod}</kbd>-drag ignores the grid · <kbd>{mod}E</kbd> split · <kbd>{mod}D</kbd> duplicate · <kbd>⌫</kbd> delete · <kbd>{mod}Z</kbd> undo · <kbd>{mod}1</kbd> Sampler
      </StatusBar>
    </div>
  );
}
