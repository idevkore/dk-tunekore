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

/** Peak output level, 0..1, about 30 times a second while sound is playing. */
export const onMeter = (handler: (peak: number) => void): Promise<UnlistenFn> =>
  listen<number>("meter", (e) => handler(e.payload));
