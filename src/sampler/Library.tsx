// The Library: one folder of samples, searchable, previewed on click or
// with the arrow keys, and dragged (or Enter / double-click) onto a pad.
import { useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { defaultLibrary, preview, scanLibrary, stopPreview, type LibraryItem, type LibraryScan } from "./api";

const ROW = 30;
const STORE = "tk.library";

interface Props {
  selectedPad: number;
  /** Puts a sample on the selected pad. */
  onLoad: (item: LibraryItem) => void;
  /** Starts dragging a sample (the parent tracks the pointer). */
  onDragStart: (item: LibraryItem, x: number, y: number) => void;
  onError: (message: string) => void;
  /** Bumped when the folder should be rescanned (a chop wrote new files). */
  refresh: number;
  /** Set when a folder is dropped on the window. */
  folder: string | null;
  onFolder: (dir: string) => void;
}

function readStore(): string | null {
  try {
    return localStorage.getItem(STORE);
  } catch {
    return null;
  }
}

function writeStore(dir: string) {
  try {
    localStorage.setItem(STORE, dir);
  } catch {
    // private window: the folder just isn't remembered
  }
}

const baseName = (p: string) => p.split(/[\\/]/).filter(Boolean).pop() ?? p;

export default function Library({ selectedPad, onLoad, onDragStart, onError, refresh, folder, onFolder }: Props) {
  const [scan, setScan] = useState<LibraryScan | null>(null);
  const [busy, setBusy] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(-1);
  const [autoPreview, setAutoPreview] = useState(true);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(400);
  const listRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  async function load(dir: string) {
    setBusy(true);
    try {
      const s = await scanLibrary(dir);
      setScan(s);
      setActive(-1);
      writeStore(dir);
      onFolder(dir);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  }

  // First folder: the one used last, else Splice's, else Music.
  useEffect(() => {
    const saved = readStore();
    if (saved) void load(saved);
    else void defaultLibrary().then((d) => { if (d) void load(d); }).catch(() => {});
  }, []);

  useEffect(() => {
    if (folder && folder !== scan?.root) void load(folder);
  }, [folder]);

  useEffect(() => {
    if (refresh > 0 && scan) void load(scan.root);
  }, [refresh]);

  useEffect(() => {
    const el = listRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setHeight(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // "/" jumps to search, as in most apps with a search box.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (e.key === "/" && !["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName)) {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const items = useMemo(() => {
    const all = scan?.items ?? [];
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    if (words.length === 0) return all;
    return all.filter((i) => {
      const hay = `${i.folder} ${i.name}`.toLowerCase();
      return words.every((w) => hay.includes(w));
    });
  }, [scan, query]);

  async function choose() {
    const dir = await open({ directory: true, title: "Choose a sample folder" }).catch(() => null);
    if (typeof dir === "string") void load(dir);
  }

  function activate(i: number, listen: boolean) {
    if (i < 0 || i >= items.length) return;
    setActive(i);
    const el = listRef.current;
    if (el) {
      if (i * ROW < el.scrollTop) el.scrollTop = i * ROW;
      else if ((i + 1) * ROW > el.scrollTop + el.clientHeight) el.scrollTop = (i + 1) * ROW - el.clientHeight;
    }
    if (listen) preview(items[i].path).catch((e) => onError(String(e)));
  }

  function onListKey(e: React.KeyboardEvent) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      activate(Math.max(0, Math.min(items.length - 1, active + (e.key === "ArrowDown" ? 1 : -1))), autoPreview);
    } else if (e.key === "Enter" && active >= 0) {
      e.preventDefault();
      onLoad(items[active]);
    } else if (e.key === "Escape") {
      void stopPreview();
    }
  }

  const first = Math.max(0, Math.floor(scrollTop / ROW) - 5);
  const last = Math.min(items.length, Math.ceil((scrollTop + height) / ROW) + 5);

  return (
    <section className="library" aria-label="Library">
      <div className="pane-head">
        <h2>Library</h2>
        <button className="btn small" onClick={() => void choose()} disabled={busy}>
          {scan ? "Change folder" : "Choose folder"}
        </button>
      </div>
      {scan && (
        <div className="folder" title={scan.root}>
          <span className="folder-name">{baseName(scan.root)}</span>
          <span className="dim mono">{busy ? "scanning…" : `${scan.items.length.toLocaleString()}${scan.truncated ? "+" : ""} files`}</span>
        </div>
      )}
      <input
        ref={searchRef}
        className="search"
        type="search"
        placeholder="Search samples  /"
        aria-label="Search samples"
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setActive(-1);
          if (listRef.current) listRef.current.scrollTop = 0;
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown") {
            e.preventDefault();
            listRef.current?.focus();
            activate(0, autoPreview);
          }
        }}
        disabled={!scan}
      />
      <div
        ref={listRef}
        className="list"
        role="listbox"
        tabIndex={0}
        aria-label="Samples"
        aria-activedescendant={active >= 0 ? `lib-${active}` : undefined}
        onKeyDown={onListKey}
        onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
      >
        {!scan && !busy && (
          <div className="list-empty">
            <strong>Pick a folder of samples</strong>
            <span>Your Splice folder, a drum kit, or anything with WAV, AIFF, FLAC or MP3 files. You can also drop a folder on the window.</span>
            <button className="btn primary" onClick={() => void choose()}>Choose folder</button>
          </div>
        )}
        {scan && items.length === 0 && !busy && (
          <div className="list-empty">
            <span>{query ? `Nothing matches “${query}”.` : "No audio files in this folder."}</span>
          </div>
        )}
        <div style={{ height: items.length * ROW, position: "relative" }}>
          {items.slice(first, last).map((item, k) => {
            const i = first + k;
            return (
              <div
                key={item.path}
                id={`lib-${i}`}
                role="option"
                aria-selected={i === active}
                className={`item${i === active ? " on" : ""}`}
                style={{ top: i * ROW, height: ROW }}
                title={`${item.folder ? item.folder + "/" : ""}${item.name}\nClick to hear it. Double-click or press Enter to put it on pad ${selectedPad + 1}, or drag it onto any pad.`}
                onPointerDown={(e) => {
                  if (e.button !== 0) return;
                  e.preventDefault();
                  listRef.current?.focus();
                  activate(i, true);
                  onDragStart(item, e.clientX, e.clientY);
                }}
                onDoubleClick={() => onLoad(item)}
              >
                <span className="item-name">{item.name}</span>
                {item.folder && <span className="item-folder">{item.folder}</span>}
              </div>
            );
          })}
        </div>
      </div>
      <label className="auto">
        <input type="checkbox" checked={autoPreview} onChange={(e) => setAutoPreview(e.target.checked)} />
        Play samples as I move through the list
      </label>
    </section>
  );
}
