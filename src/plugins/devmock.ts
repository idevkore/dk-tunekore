// Dev only: stands in for the plugins' Rust side (src-tauri/src/plugins/) at
// http://localhost:1420/?mock, on top of the Sampler's and the Timeline's
// mocks: it wraps the mocked invoke and answers the `plugins_*` commands with
// a made-up plugin list. main.tsx loads it only in `npm run dev` with `?mock`.
import { emit } from "@tauri-apps/api/event";
import type { CatalogEntry, CatalogView, DeviceView, PluginsView } from "./api";

type Invoke = (cmd: string, args?: unknown, options?: unknown) => Promise<unknown>;

const entry = (name: string, vendor: string, kind: "instrument" | "effect", subCategories: string[]): CatalogEntry => ({
  path: `/Library/Audio/Plug-Ins/VST3/${name}.vst3`,
  classId: name.toUpperCase().replace(/\W/g, "").padEnd(32, "0").slice(0, 32),
  name,
  vendor,
  version: "1.0",
  kind,
  subCategories,
});

export function installMocks() {
  const internals = (window as unknown as { __TAURI_INTERNALS__: { invoke: Invoke } }).__TAURI_INTERNALS__;
  const inner = internals.invoke;
  const catalog: CatalogView = {
    plugins: [
      entry("Serum 2", "Xfer Records", "instrument", ["Instrument", "Synth"]),
      entry("Serum", "Xfer Records", "instrument", ["Instrument", "Synth"]),
      entry("Jup-8 V4", "Arturia", "instrument", ["Instrument", "Synth"]),
      entry("Kontakt 7", "Native Instruments", "instrument", ["Instrument", "Sampler"]),
      entry("Guitar Rig 7", "Native Instruments", "effect", ["Fx", "Distortion"]),
      entry("Efx REFRACT", "Arturia", "effect", ["Fx", "Modulation"]),
    ],
    failed: [{ name: "Broken Synth", path: "/Library/Audio/Plug-Ins/VST3/Broken.vst3", status: "crashed", error: "it crashed while loading" }],
    scanning: false,
    error: null,
  };
  const v: PluginsView = { devices: [], focus: null };
  let nextId = 1;
  const send = () => {
    void emit("plugins:state", v);
    return structuredClone(v);
  };
  const find = (id: number) => {
    const d = v.devices.find((x) => x.id === id);
    if (!d) throw new Error(`there is no plugin ${id}`);
    return d;
  };
  internals.invoke = async (cmd: string, args?: unknown, options?: unknown) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "plugins_state":
        return structuredClone(v);
      case "plugins_catalog":
        return structuredClone(catalog);
      case "plugins_scan":
        catalog.scanning = true;
        void emit("plugins:catalog", catalog);
        setTimeout(() => {
          catalog.scanning = false;
          void emit("plugins:catalog", catalog);
        }, 800);
        return null;
      case "plugins_add": {
        const ref = a.plugin as { path: string; classId: string };
        const p = catalog.plugins.find((x) => x.path === ref.path && x.classId === ref.classId);
        if (!p) throw new Error("no such plugin");
        let track = a.track as number | null;
        const pad = (a.pad as number | null) ?? null;
        if (pad !== null) {
          if (p.kind !== "instrument") throw new Error("a pad plays an instrument; put effects on the Sampler's track");
          track = 1;
          v.devices = v.devices.filter((x) => !(x.role === "pad" && x.pad === pad));
        } else if (track === null) {
          if (p.kind !== "instrument") throw new Error("choose a track for the effect");
          // A new instrument track, through the Timeline's mock.
          const tl = (await inner("timeline_track_add")) as { tracks: { id: number }[] };
          track = tl.tracks[tl.tracks.length - 1].id;
          await inner("timeline_track_set", { id: track, change: { name: p.name, kind: "instrument" } });
        }
        const d: DeviceView = {
          id: nextId++,
          track,
          role: pad !== null ? "pad" : p.kind === "instrument" ? "instrument" : "effect",
          pad: pad ?? 0,
          name: p.name,
          vendor: p.vendor,
          status: "ok",
          error: null,
          bypass: false,
          hasEditor: true,
          editorOpen: false,
          latency: p.name === "Guitar Rig 7" ? 64 : 0,
        };
        if (d.role === "instrument") v.devices = v.devices.filter((x) => !(x.track === track && x.role === "instrument"));
        const index = a.index as number | null;
        const effects = v.devices.filter((x) => x.track === track && x.role === "effect");
        if (d.role === "effect" && index !== null && index < effects.length) v.devices.splice(v.devices.indexOf(effects[index]), 0, d);
        else v.devices.push(d);
        return send();
      }
      case "plugins_remove":
        v.devices = v.devices.filter((x) => x.id !== a.id);
        return send();
      case "plugins_remove_track":
        v.devices = v.devices.filter((x) => x.track !== a.track);
        await inner("timeline_track_remove", { id: a.track });
        return send();
      case "plugins_bypass":
        find(a.id as number).bypass = a.on as boolean;
        return send();
      case "plugins_move": {
        const d = find(a.id as number);
        const effects = v.devices.filter((x) => x.track === d.track && x.role === "effect");
        const to = Math.min(a.to as number, effects.length - 1);
        v.devices.splice(v.devices.indexOf(d), 1);
        const rest = v.devices.filter((x) => x.track === d.track && x.role === "effect");
        const at = to < rest.length ? v.devices.indexOf(rest[to]) : rest.length ? v.devices.indexOf(rest[rest.length - 1]) + 1 : v.devices.length;
        v.devices.splice(at, 0, d);
        return send();
      }
      case "plugins_editor":
        find(a.id as number).editorOpen = a.open as boolean;
        return send();
      case "plugins_reload": {
        const d = find(a.id as number);
        d.status = "ok";
        d.error = null;
        return send();
      }
      case "plugins_focus":
        v.focus = (a.track as number | null) ?? null;
        return structuredClone(v);
      default:
        return inner(cmd, args, options);
    }
  };
}
