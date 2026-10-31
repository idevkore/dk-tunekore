//! Plugin parameters as MIDI learn targets: `plugin.<device id>.<param id>`,
//! each on its normalized value, 0 to 1.
//!
//! MIDI learn asks from its own threads, and a plugin's controller lives on
//! the main thread, so the targets read a copy of each device's parameters
//! ([`ParamCache`]) that the main thread keeps up to date, and a move is
//! queued for the main thread rather than waited for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Manager};
use tunekore_engine::plugins::{ParamId, PluginController};
use tunekore_engine::TrackId;

use crate::midi::map::{Curve, Move, TargetInfo};
use crate::midi::Targets;

#[derive(Debug, Clone)]
struct Param {
    id: ParamId,
    name: String,
    steps: u32,
    value: f64,
}

#[derive(Debug, Clone)]
struct DeviceParams {
    plugin: String,
    track: TrackId,
    track_name: String,
    params: Vec<Param>,
}

/// Each device's learnable parameters and their values, by device id.
#[derive(Default)]
pub struct ParamCache {
    devices: Mutex<HashMap<u64, DeviceParams>>,
    /// Moves waiting for the main thread, the latest value of each (device,
    /// parameter): a knob turning faster than the main thread runs sends one.
    moves: Mutex<HashMap<(u64, ParamId), f64>>,
    /// Whether a run of `moves` is already queued.
    queued: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ParamCache {
    /// Main thread: reads device `id`'s parameters (the automatable ones
    /// that aren't hidden or read-only) and their values.
    pub fn store(&self, id: u64, plugin: &str, track: TrackId, controller: &mut dyn PluginController) {
        let list: Vec<(ParamId, String, u32)> = controller
            .params()
            .iter()
            .filter(|p| p.automatable && !p.hidden && !p.read_only && !p.is_bypass)
            .map(|p| (p.id, p.name.clone(), p.step_count))
            .collect();
        let params = list.into_iter().map(|(pid, name, steps)| Param { id: pid, name, steps, value: controller.param_value(pid).unwrap_or(0.0) }).collect();
        let mut d = lock(&self.devices);
        let track_name = d.get(&id).map(|o| o.track_name.clone()).unwrap_or_default();
        d.insert(id, DeviceParams { plugin: plugin.to_string(), track, track_name, params });
    }

    pub fn forget(&self, id: u64) {
        lock(&self.devices).remove(&id);
    }

    pub fn set_value(&self, id: u64, param: ParamId, value: f64) {
        if let Some(p) = lock(&self.devices).get_mut(&id).and_then(|d| d.params.iter_mut().find(|p| p.id == param)) {
            p.value = value;
        }
    }

    /// Keeps the track names in the targets' groups current.
    pub fn track_names(&self, names: &HashMap<TrackId, &str>) {
        for d in lock(&self.devices).values_mut() {
            if let Some(n) = names.get(&d.track) {
                if d.track_name != *n {
                    d.track_name = n.to_string();
                }
            }
        }
    }

    fn info(d: &DeviceParams, device: u64, p: &Param) -> TargetInfo {
        let group = if d.track_name.is_empty() { d.plugin.clone() } else { format!("{} · {}", d.track_name, d.plugin) };
        TargetInfo {
            id: format!("plugin.{device}.{}", p.id),
            label: p.name.clone(),
            group,
            curve: Curve::Linear { min: 0.0, max: 1.0 },
            step: if p.steps > 0 { 1.0 / f64::from(p.steps) } else { 0.0 },
            centered: false,
        }
    }
}

/// `plugin.<device>.<param>` as numbers.
fn parse(id: &str) -> Option<(u64, ParamId)> {
    let mut parts = id.split('.');
    if parts.next()? != "plugin" {
        return None;
    }
    let device = parts.next()?.parse().ok()?;
    let param = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((device, param))
}

pub struct PluginTargets {
    cache: Arc<ParamCache>,
}

impl PluginTargets {
    pub fn new(cache: Arc<ParamCache>) -> Self {
        Self { cache }
    }
}

impl Targets<AppHandle> for PluginTargets {
    fn prefixes(&self) -> &'static [&'static str] {
        &["plugin"]
    }

    fn list(&self, _: &AppHandle) -> Vec<TargetInfo> {
        let d = lock(&self.cache.devices);
        let mut ids: Vec<&u64> = d.keys().collect();
        ids.sort();
        ids.into_iter().flat_map(|id| d[id].params.iter().map(|p| ParamCache::info(&d[id], *id, p))).collect()
    }

    fn info(&self, _: &AppHandle, id: &str) -> Option<TargetInfo> {
        let (device, param) = parse(id)?;
        let d = lock(&self.cache.devices);
        let dev = d.get(&device)?;
        dev.params.iter().find(|p| p.id == param).map(|p| ParamCache::info(dev, device, p))
    }

    fn get(&self, _: &AppHandle, id: &str) -> Option<f64> {
        let (device, param) = parse(id)?;
        lock(&self.cache.devices).get(&device)?.params.iter().find(|p| p.id == param).map(|p| p.value)
    }

    /// Queues the moves for the main thread (the plugins' controllers live there).
    fn set(&self, app: &AppHandle, moves: &[Move], _undo_key: &str) -> Result<(), String> {
        let list: Vec<(u64, ParamId, f64)> = moves.iter().filter_map(|m| parse(&m.target).map(|(d, p)| (d, p, m.value.clamp(0.0, 1.0)))).collect();
        if list.is_empty() {
            return Ok(());
        }
        {
            let mut waiting = lock(&self.cache.moves);
            for &(d, p, v) in &list {
                self.cache.set_value(d, p, v);
                waiting.insert((d, p), v);
            }
        }
        app.state::<super::Plugins>().revision.fetch_add(1, Ordering::Relaxed);
        if !self.cache.queued.swap(true, Ordering::AcqRel) {
            let cache = self.cache.clone();
            super::later(app, move |_, doc| {
                // Cleared first: a move that lands while these apply queues another run.
                cache.queued.store(false, Ordering::Release);
                let moves = std::mem::take(&mut *lock(&cache.moves));
                for ((device, param), value) in moves {
                    let Ok(i) = doc.find(device) else { continue };
                    if let Some(l) = &mut doc.slots[i].loaded {
                        l.controller.set_param(param, value);
                    }
                }
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_a_device_and_a_parameter() {
        assert_eq!(parse("plugin.3.1024"), Some((3, 1024)));
        assert_eq!(parse("plugin.3"), None);
        assert_eq!(parse("plugin.x.1"), None);
        assert_eq!(parse("plugin.1.2.3"), None);
        assert_eq!(parse("track.1.volume"), None);
    }
}
