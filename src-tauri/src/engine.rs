//! Safe wrapper over the C++ engine's C interface (engine/include/musikore/engine.h).

use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_float, c_int, c_uint, CStr};
use std::sync::Mutex;
use std::time::Duration;

#[allow(non_camel_case_types)]
#[repr(C)]
struct mk_engine {
    _private: [u8; 0],
}

extern "C" {
    fn mk_engine_create() -> *mut mk_engine;
    fn mk_engine_destroy(engine: *mut mk_engine);
    fn mk_engine_version() -> *const c_char;
    fn mk_engine_list_outputs_json(engine: *mut mk_engine, buf: *mut c_char, cap: usize) -> usize;
    fn mk_engine_open_output(
        engine: *mut mk_engine,
        device_id: c_uint,
        sample_rate: c_uint,
        buffer_frames: c_uint,
    ) -> c_int;
    fn mk_engine_close_output(engine: *mut mk_engine);
    fn mk_engine_set_tone(engine: *mut mk_engine, on: c_int, freq_hz: c_float, gain: c_float);
    fn mk_engine_take_peak(engine: *mut mk_engine) -> c_float;
    fn mk_engine_last_error(engine: *mut mk_engine) -> *const c_char;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputDevice {
    pub id: u32,
    pub name: String,
    pub channels: u32,
    pub sample_rates: Vec<u32>,
    pub preferred_rate: u32,
    pub is_default: bool,
}

/// Test tone level. Quiet on purpose.
const TONE_GAIN: f32 = 0.2;
/// Longer than the engine's 10 ms gain ramp, so the stream closes on silence.
const RAMP_WAIT: Duration = Duration::from_millis(40);

pub struct Engine {
    ptr: *mut mk_engine,
    /// Serializes the control calls; the engine expects one caller at a time.
    /// Holds the device the stream is open on, if any.
    open_device: Mutex<Option<u32>>,
}

// The engine's control calls are serialized by `open_device`; `set_tone` and
// `take_peak` only touch atomics inside the engine.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    pub fn new() -> Result<Self, String> {
        let ptr = unsafe { mk_engine_create() };
        if ptr.is_null() {
            return Err("could not create the audio engine".into());
        }
        Ok(Self { ptr, open_device: Mutex::new(None) })
    }

    pub fn version() -> String {
        unsafe { CStr::from_ptr(mk_engine_version()) }.to_string_lossy().into_owned()
    }

    fn last_error(&self) -> String {
        let msg = unsafe { CStr::from_ptr(mk_engine_last_error(self.ptr)) };
        let msg = msg.to_string_lossy();
        if msg.is_empty() { "audio engine error".into() } else { msg.into_owned() }
    }

    pub fn list_outputs(&self) -> Result<Vec<OutputDevice>, String> {
        let _guard = self.open_device.lock().unwrap();
        let mut buf = vec![0u8; 8192];
        let mut need =
            unsafe { mk_engine_list_outputs_json(self.ptr, buf.as_mut_ptr().cast(), buf.len()) };
        if need >= buf.len() {
            buf = vec![0u8; need + 1];
            need = unsafe { mk_engine_list_outputs_json(self.ptr, buf.as_mut_ptr().cast(), buf.len()) };
        }
        serde_json::from_slice(&buf[..need.min(buf.len() - 1)]).map_err(|e| e.to_string())
    }

    /// Starts the tone, opening the device first if it is not already open.
    /// `device` of `None` means the system default. Calling it again with a new
    /// frequency retunes the running tone.
    pub fn start_tone(&self, device: Option<u32>, freq_hz: f32) -> Result<(), String> {
        let mut open = self.open_device.lock().unwrap();
        let want = device.unwrap_or(0);
        if *open != Some(want) {
            if open.is_some() {
                self.fade_out_and_close();
            }
            let rc = unsafe { mk_engine_open_output(self.ptr, want, 0, 0) };
            if rc != 0 {
                *open = None;
                return Err(self.last_error());
            }
            *open = Some(want);
        }
        unsafe { mk_engine_set_tone(self.ptr, 1, freq_hz, TONE_GAIN) };
        Ok(())
    }

    pub fn stop_tone(&self) {
        let mut open = self.open_device.lock().unwrap();
        if open.take().is_some() {
            self.fade_out_and_close();
        }
    }

    fn fade_out_and_close(&self) {
        unsafe { mk_engine_set_tone(self.ptr, 0, 440.0, 0.0) };
        std::thread::sleep(RAMP_WAIT);
        unsafe { mk_engine_close_output(self.ptr) };
    }

    /// Peak output level since the last call, 0..1. Safe from any thread.
    pub fn take_peak(&self) -> f32 {
        unsafe { mk_engine_take_peak(self.ptr) }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { mk_engine_destroy(self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_version() {
        assert!(!Engine::version().is_empty());
    }

    /// Plays a tone on the default output and checks the audio callback saw it.
    /// Skips when the machine has no output device, and on CI runners.
    #[test]
    fn tone_reaches_the_audio_callback() {
        if std::env::var_os("CI").is_some() {
            eprintln!("CI has no sound card; skipping");
            return;
        }
        let engine = Engine::new().unwrap();
        let outputs = engine.list_outputs().unwrap();
        if outputs.is_empty() {
            eprintln!("no output devices; skipping");
            return;
        }
        if engine.start_tone(None, 440.0).is_err() {
            eprintln!("could not open the default output; skipping");
            return;
        }
        engine.take_peak();
        std::thread::sleep(Duration::from_millis(400));
        let peak = engine.take_peak();
        assert!(peak > 0.15 && peak <= TONE_GAIN + 0.001, "peak was {peak}");

        engine.stop_tone();
        std::thread::sleep(Duration::from_millis(50));
        engine.take_peak();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(engine.take_peak(), 0.0);
    }
}
