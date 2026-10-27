// Typed wrappers for the commands the Rust side exposes (src-tauri/src/lib.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface OutputDevice {
  /** cpal's device id, e.g. "coreaudio:BuiltInSpeakerDevice". */
  id: string;
  name: string;
  channels: number;
  sampleRates: number[];
  preferredRate: number;
  isDefault: boolean;
}

export const engineVersion = () => invoke<string>("engine_version");
export const listOutputs = () => invoke<OutputDevice[]>("list_outputs");
export const startTone = (deviceId: string | null, frequency: number) =>
  invoke<void>("start_tone", { deviceId, frequency });
export const stopTone = () => invoke<void>("stop_tone");

/** Interface zoom, 1 = 100%. Also in the View menu (⌘+, ⌘−, ⌘0). */
export const getDisplayZoom = () => invoke<number>("display_zoom");
export const setDisplayZoom = (zoom: number) => invoke<void>("set_display_zoom", { zoom });
export const ZOOM_LEVELS = [0.8, 0.9, 1, 1.1, 1.25, 1.35, 1.5];

export const setMaster = (db: number) => invoke<void>("set_master", { db });
export const getMaster = () => invoke<number>("master");
/** The master limiter's gain reduction in dB (negative) while it works. */
export const onLimiter = (handler: (db: number) => void): Promise<UnlistenFn> =>
  listen<number>("limiter", (e) => handler(e.payload));

/** Peak output level, 0..1, about 30 times a second while sound is playing. */
export const onMeter = (handler: (peak: number) => void): Promise<UnlistenFn> =>
  listen<number>("meter", (e) => handler(e.payload));
