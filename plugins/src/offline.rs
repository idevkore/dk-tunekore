//! Renders a plugin to memory faster than real time: for bounces, for tests, and for checking a
//! plugin from the command line.

use crate::plugin::*;
use std::time::Instant;

/// A note to play, in beats from the start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    pub start_beat: f64,
    pub length_beats: f64,
    pub key: u8,
    pub velocity: f32,
    pub channel: u8,
}

/// What to feed an effect's inputs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Silence,
    /// A sine wave on every input channel.
    Sine { hz: f64, level: f32 },
}

#[derive(Debug, Clone)]
pub struct RenderSpec {
    pub sample_rate: f64,
    pub block_size: usize,
    pub seconds: f64,
    pub tempo: f64,
    pub notes: Vec<Note>,
    pub input: Input,
    pub inputs: BusSelection,
    pub outputs: BusSelection,
    /// Parameter changes at the start of the render, as (id, normalized value).
    pub params: Vec<(ParamId, f64)>,
}

impl Default for RenderSpec {
    fn default() -> Self {
        RenderSpec { sample_rate: 48_000.0, block_size: 512, seconds: 4.0, tempo: 120.0, notes: Vec::new(), input: Input::Silence, inputs: BusSelection::Main, outputs: BusSelection::Main, params: Vec::new() }
    }
}

impl RenderSpec {
    /// C4, E4, G4, C5, one note per beat and each half a beat long: the pattern the hosting
    /// spike used.
    pub fn arpeggio() -> Vec<Note> {
        [60u8, 64, 67, 72].iter().enumerate().map(|(i, &key)| Note { start_beat: i as f64, length_beats: 0.5, key, velocity: 0.8, channel: 0 }).collect()
    }
}

/// What a render produced.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The buses in force.
    pub layout: BusLayout,
    /// Every active output channel, bus after bus (see [`BusLayout::output_range`]).
    pub channels: Vec<Vec<f32>>,
    pub blocks: usize,
    /// Slowest and mean time to process one block, in microseconds.
    pub max_block_micros: u64,
    pub mean_block_micros: u64,
    /// Notes the plugin sent out.
    pub midi_out: usize,
}

impl Rendered {
    pub fn peak(&self) -> f32 {
        self.channels.iter().flatten().fold(0.0f32, |m, &x| m.max(x.abs()))
    }

    /// Peak of the channels of output bus `bus`.
    pub fn bus_peak(&self, bus: usize) -> f32 {
        self.layout.output_range(bus).filter_map(|c| self.channels.get(c)).flatten().fold(0.0f32, |m, &x| m.max(x.abs()))
    }

    pub fn rms(&self) -> f64 {
        let (mut sum, mut n) = (0.0f64, 0usize);
        for &x in self.channels.iter().flatten() {
            sum += f64::from(x) * f64::from(x);
            n += 1;
        }
        if n == 0 { 0.0 } else { (sum / n as f64).sqrt() }
    }
}

/// Peak level in dBFS (`-inf` for silence).
pub fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// Largest difference between two renders' samples (`INFINITY` if their shapes differ).
pub fn max_difference(a: &Rendered, b: &Rendered) -> f32 {
    if a.channels.len() != b.channels.len() {
        return f32::INFINITY;
    }
    let mut worst = 0.0f32;
    for (x, y) in a.channels.iter().zip(&b.channels) {
        if x.len() != y.len() {
            return f32::INFINITY;
        }
        worst = x.iter().zip(y).fold(worst, |m, (p, q)| m.max((p - q).abs()));
    }
    worst
}

/// Activates `controller` for an offline render, plays `spec` through it and returns the audio.
/// Leaves the plugin activated.
pub fn render(controller: &mut dyn PluginController, spec: &RenderSpec) -> Result<Rendered, PluginError> {
    let setup = ProcessSetup {
        sample_rate: spec.sample_rate,
        max_block_size: spec.block_size as u32,
        realtime: false,
        inputs: spec.inputs.clone(),
        outputs: spec.outputs.clone(),
        main_channels: 2,
    };
    let mut plugin = controller.activate(&setup)?;
    let layout = plugin.layout().clone();
    for &(id, v) in &spec.params {
        controller.set_param(id, v);
    }
    let total = (spec.seconds * spec.sample_rate).round() as usize;
    let (n_in, n_out) = (layout.input_channels(), layout.output_channels());
    let mut in_bufs = vec![vec![0.0f32; spec.block_size]; n_in];
    let mut out_bufs = vec![vec![0.0f32; spec.block_size]; n_out];
    let mut channels = vec![Vec::with_capacity(total); n_out];
    let mut table = ChannelTable::with_capacity(n_in, n_out);
    let samples_per_beat = 60.0 / spec.tempo * spec.sample_rate;
    // Note on/off times in samples.
    let mut edges: Vec<(usize, Event)> = Vec::new();
    for (i, n) in spec.notes.iter().enumerate() {
        let id = i as i32;
        let on = (n.start_beat * samples_per_beat).round() as usize;
        let off = ((n.start_beat + n.length_beats) * samples_per_beat).round() as usize;
        edges.push((on, Event { offset: 0, kind: EventKind::NoteOn { channel: n.channel, key: n.key, velocity: n.velocity, note_id: id } }));
        edges.push((off, Event { offset: 0, kind: EventKind::NoteOff { channel: n.channel, key: n.key, velocity: 0.0, note_id: id } }));
    }
    edges.sort_by_key(|e| e.0);
    let mut events: Vec<Event> = Vec::with_capacity(256);
    let (mut midi_out, mut param_out) = (OutBuf::<Event>::with_capacity(256), OutBuf::<ParamChange>::with_capacity(256));
    let (mut blocks, mut max_us, mut sum_us, mut sent) = (0usize, 0u64, 0u64, 0usize);
    let mut pos = 0usize;
    while pos < total {
        let frames = spec.block_size.min(total - pos);
        events.clear();
        for (at, e) in edges.iter().filter(|(at, _)| *at >= pos && *at < pos + frames) {
            events.push(Event { offset: (*at - pos) as u32, ..*e });
        }
        if let Input::Sine { hz, level } = spec.input {
            for ch in in_bufs.iter_mut() {
                for (i, x) in ch.iter_mut().enumerate() {
                    let t = (pos + i) as f64 / spec.sample_rate;
                    *x = level * (2.0 * std::f64::consts::PI * hz * t).sin() as f32;
                }
            }
        }
        let beats = pos as f64 / samples_per_beat;
        let transport = Transport {
            playing: true,
            tempo: spec.tempo,
            project_samples: pos as i64,
            continuous_samples: pos as i64,
            project_beats: beats,
            bar_start_beats: (beats / 4.0).floor() * 4.0,
            ..Transport::default()
        };
        midi_out.clear();
        param_out.clear();
        let (inputs, outputs) = table.audio(&in_bufs, &mut out_bufs);
        let mut block = ProcessBlock {
            frames,
            inputs,
            outputs,
            events: &events,
            param_changes: &[],
            transport: &transport,
            midi_out: &mut midi_out,
            param_out: &mut param_out,
        };
        let t0 = Instant::now();
        plugin.process(&mut block).map_err(|e| PluginError::Failed(format!("process failed at sample {pos}: {e}{}", controller.take_error().map(|m| format!(" ({m})")).unwrap_or_default())))?;
        let us = t0.elapsed().as_micros() as u64;
        max_us = max_us.max(us);
        sum_us += us;
        sent += midi_out.as_slice().len();
        for (dst, src) in channels.iter_mut().zip(&out_bufs) {
            dst.extend_from_slice(&src[..frames]);
        }
        blocks += 1;
        pos += frames;
    }
    Ok(Rendered { layout, channels, blocks, max_block_micros: max_us, mean_block_micros: if blocks == 0 { 0 } else { sum_us / blocks as u64 }, midi_out: sent })
}
