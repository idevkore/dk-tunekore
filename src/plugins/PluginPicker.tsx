// The plugin list to add from: the scanner's plugins of one kind (instruments
// or effects), searchable by name, vendor or category, with Rescan and the
// plugins that failed to load. ↑ and ↓ move through the list, Enter adds.
import { useEffect, useMemo, useRef, useState } from "react";
import Dialog from "../sampler/Dialog";
import * as api from "./api";
import { subscribe, type CatalogEntry, type CatalogView } from "./api";
import "./plugins.css";

type Props = {
  /** Which plugins to list. */
  kind: "instrument" | "effect";
  /** The dialog's title: "Add an effect to Keys". */
  title: string;
  onPick: (p: CatalogEntry) => void;
  onClose: () => void;
};

const STATUS: Record<string, string> = { failed: "failed to load", crashed: "crashed while loading", timed_out: "took too long to load" };

export default function PluginPicker({ kind, title, onPick, onClose }: Props) {
  const [catalog, setCatalog] = useState<CatalogView | null>(null);
  const [query, setQuery] = useState("");
  const [at, setAt] = useState(0);
  const [showFailed, setShowFailed] = useState(false);
  const list = useRef<HTMLUListElement>(null);

  useEffect(() => {
    api.catalog().then(setCatalog).catch(() => {});
    return subscribe(api.onCatalog(setCatalog));
  }, []);

  const shown = useMemo(() => {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    return (catalog?.plugins ?? [])
      .filter((p) => (kind === "instrument" ? p.kind === "instrument" : p.kind !== "instrument"))
      .filter((p) => {
        const hay = `${p.name} ${p.vendor} ${p.subCategories.join(" ")}`.toLowerCase();
        return words.every((w) => hay.includes(w));
      });
  }, [catalog, query, kind]);

  useEffect(() => setAt(0), [query]);
  useEffect(() => {
    list.current?.querySelector<HTMLElement>(`[data-i="${at}"]`)?.scrollIntoView({ block: "nearest" });
  }, [at]);

  const pick = (p: CatalogEntry | undefined) => p && onPick(p);
  const scanning = catalog?.scanning ?? true;
  const failed = catalog?.failed ?? [];

  return (
    <Dialog title={title} onClose={onClose}>
      <div className="pp">
        <input
          className="search"
          type="search"
          placeholder={kind === "instrument" ? "Search instruments" : "Search effects"}
          aria-label="Search plugins"
          value={query}
          autoFocus
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setAt((i) => Math.min(shown.length - 1, i + 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setAt((i) => Math.max(0, i - 1));
            } else if (e.key === "Enter") {
              e.preventDefault();
              pick(shown[at]);
            }
          }}
        />
        <ul className="pp-list" ref={list} role="listbox" aria-label={kind === "instrument" ? "Instruments" : "Effects"}>
          {shown.map((p, i) => (
            <li key={`${p.path}#${p.classId}`} data-i={i} role="option" aria-selected={i === at} className={i === at ? "on" : undefined} onMouseEnter={() => setAt(i)} onClick={() => pick(p)}>
              <span className="pp-name">{p.name}</span>
              <span className="pp-vendor">{p.vendor}</span>
              <span className="pp-sub">{p.subCategories.filter((c) => c !== "Instrument" && c !== "Fx").join(" · ")}</span>
            </li>
          ))}
          {shown.length === 0 && (
            <li className="pp-empty" aria-disabled="true">
              {scanning ? "Looking for plugins…" : query ? "Nothing matches." : `No ${kind === "instrument" ? "instruments" : "effects"} found in the VST3 folders.`}
            </li>
          )}
        </ul>
        {catalog?.error && <p className="pp-error">{catalog.error}</p>}
        {failed.length > 0 && (
          <div className="pp-failed">
            <button className="pp-failed-head" aria-expanded={showFailed} onClick={() => setShowFailed((s) => !s)}>
              {failed.length === 1 ? "1 plugin didn't load" : `${failed.length} plugins didn't load`}
              <span aria-hidden="true">{showFailed ? "▾" : "▸"}</span>
            </button>
            {showFailed && (
              <ul>
                {failed.map((f) => (
                  <li key={f.path + f.name}>
                    <span className="pp-name">{f.name}</span>
                    <span className="pp-sub">
                      {STATUS[f.status] ?? f.status}
                      {f.error ? `: ${f.error}` : ""}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
      <div className="dialog-foot">
        <span className="dim pp-count">{scanning ? "Scanning…" : `${shown.length} ${kind === "instrument" ? "instruments" : "effects"}`}</span>
        {failed.length > 0 && (
          <button className="btn small" disabled={scanning} onClick={() => void api.scan("retry")} title="Give the plugins that failed another try (after fixing a licence, say)">
            Retry failed
          </button>
        )}
        <button className="btn small" disabled={scanning} onClick={() => void api.scan("rescan")} title="Look at every plugin again">
          Rescan
        </button>
        <button className="btn small" onClick={onClose}>
          Cancel
        </button>
      </div>
    </Dialog>
  );
}
