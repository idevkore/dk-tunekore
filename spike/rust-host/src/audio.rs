//! Runs a renderer either on the default output device (cpal) or offline.

use crate::meter::Meter;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::time::Instant;

pub const MAX_BLOCK: usize = 4096;

/// Renders `left.len()` frames of planar stereo. Called on the audio thread.
pub trait Renderer: Send {
    fn render(&mut self, left: &mut [f32], right: &mut [f32]);
}

pub fn device_rate() -> Result<(cpal::Device, cpal::StreamConfig), String> {
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no output device")?;
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("device format is {:?}, spike wants f32", cfg.sample_format()));
    }
    let mut cfg: cpal::StreamConfig = cfg.into();
    cfg.buffer_size = cpal::BufferSize::Fixed(512);
    Ok((dev, cfg))
}

pub fn play(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    mut renderer: Box<dyn Renderer>,
    meter: Arc<Meter>,
) -> Result<cpal::Stream, String> {
    let ch = cfg.channels as usize;
    let rate = cfg.sample_rate as f64;
    let mut left = vec![0.0f32; MAX_BLOCK];
    let mut right = vec![0.0f32; MAX_BLOCK];
    let stream = dev
        .build_output_stream(
            cfg.clone(),
            move |out: &mut [f32], _| {
                let t0 = Instant::now();
                let mut done = 0;
                let frames = out.len() / ch;
                while done < frames {
                    let n = (frames - done).min(MAX_BLOCK);
                    let (l, r) = (&mut left[..n], &mut right[..n]);
                    renderer.render(l, r);
                    for i in 0..n {
                        let o = &mut out[(done + i) * ch..(done + i + 1) * ch];
                        o[0] = l[i];
                        if ch > 1 { o[1] = r[i]; }
                        for x in o.iter_mut().skip(2) { *x = 0.0; }
                    }
                    meter.record(l, r);
                    done += n;
                }
                let load = t0.elapsed().as_secs_f64() / (frames as f64 / rate);
                meter.record_load(load as f32);
            },
            |e| eprintln!("stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Renders `seconds` of audio as fast as possible, in 512-frame blocks.
pub fn offline(mut renderer: Box<dyn Renderer>, meter: &Meter, rate: f64, seconds: f64) {
    let mut left = vec![0.0f32; 512];
    let mut right = vec![0.0f32; 512];
    let blocks = (rate * seconds / 512.0) as usize;
    for _ in 0..blocks {
        let t0 = Instant::now();
        renderer.render(&mut left, &mut right);
        let load = t0.elapsed().as_secs_f64() / (512.0 / rate);
        meter.record(&left, &right);
        meter.record_load(load as f32);
    }
}
