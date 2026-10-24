//! Decodes a song file to planar f32 with Symphonia: WAV, AIFF, FLAC, MP3,
//! and M4A (AAC or ALAC).

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// File extensions the Analyzer opens.
pub const EXTENSIONS: [&str; 7] = ["wav", "aif", "aiff", "flac", "mp3", "m4a", "mp4"];

/// A decoded song: `channels[c][frame]`, one or two channels.
#[derive(Debug, Clone)]
pub struct Song {
    pub sample_rate: u32,
    pub channels: Arc<Vec<Vec<f32>>>,
    /// The codec, for display ("FLAC", "AAC"...).
    pub codec: String,
    /// Bits per sample in the file, when the format has one.
    pub bits: Option<u32>,
}

impl Song {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }
}

pub fn decode_file(path: &Path) -> Result<Song, String> {
    let file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("not an audio file musiKore can read ({e})"))?;
    let track = format.default_track(TrackType::Audio).ok_or("the file has no audio track")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("the audio track has no codec parameters")?;
    let Some(codec) =
        symphonia::default::get_codecs().get_audio_decoder(params.codec).map(|d| d.codec.info.short_name.to_uppercase())
    else {
        return Err(format!("musiKore can't decode this file's codec ({}); WAV, AIFF, FLAC, MP3, AAC and ALAC work", params.codec));
    };
    let bits = params.bits_per_sample;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| format!("can't decode this file's audio ({e})"))?;
    let track_id = track.id;

    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut rate = 0u32;
    let mut block: Vec<Vec<f32>> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(format!("read error: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(Error::DecodeError(_)) | Err(Error::IoError(_)) => continue, // skip a damaged packet
            Err(e) => return Err(format!("decode error: {e}")),
        };
        if rate == 0 {
            rate = buf.spec().rate();
        }
        buf.copy_to_vecs_planar::<f32>(&mut block);
        if channels.is_empty() {
            channels = vec![Vec::new(); block.len()];
        }
        for (dst, src) in channels.iter_mut().zip(&block) {
            dst.extend_from_slice(src);
        }
    }
    if channels.is_empty() || channels[0].is_empty() || rate == 0 {
        return Err("the file has no audio in it".into());
    }
    // Stems are stereo at most. A surround file keeps its front left and right.
    channels.truncate(2);
    Ok(Song { sample_rate: rate, channels: Arc::new(channels), codec, bits })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, rate: u32, channels: u16, frames: usize) -> Vec<Vec<f32>> {
        let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let mut planar = vec![Vec::new(); channels as usize];
        for i in 0..frames {
            for (c, p) in planar.iter_mut().enumerate() {
                let v = ((i as f32 * 0.01 * (c + 1) as f32).sin() * 0.5 * 8_388_607.0).round() as i32;
                w.write_sample(v).unwrap();
                p.push(v as f32 / 8_388_608.0);
            }
        }
        w.finalize().unwrap();
        planar
    }

    #[test]
    fn decodes_a_wav_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let want = write_wav(&path, 44_100, 2, 10_000);
        let song = decode_file(&path).unwrap();
        assert_eq!(song.sample_rate, 44_100);
        assert_eq!(song.channels.len(), 2);
        assert_eq!(song.frames(), 10_000);
        let err = song.channels[1].iter().zip(&want[1]).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(err < 1e-6, "{err}");
        assert_eq!(song.bits, Some(24));
    }

    #[test]
    fn rejects_what_is_not_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mp3");
        std::fs::write(&path, b"not audio at all").unwrap();
        assert!(decode_file(&path).is_err());
    }

    /// The other formats, encoded by ffmpeg when this machine has it.
    #[test]
    fn decodes_every_format_the_analyzer_opens() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("src.wav");
        write_wav(&wav, 48_000, 2, 48_000);
        for (ext, args) in [
            ("aiff", vec![]),
            ("flac", vec![]),
            ("mp3", vec!["-b:a", "192k"]),
            ("m4a", vec!["-c:a", "aac", "-b:a", "192k"]),
        ] {
            let out = dir.path().join(format!("t.{ext}"));
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-i"])
                .arg(&wav)
                .args(&args)
                .arg(&out)
                .status()
                .unwrap()
                .success();
            assert!(ok, "ffmpeg failed for {ext}");
            let song = decode_file(&out).unwrap_or_else(|e| panic!("{ext}: {e}"));
            assert_eq!(song.sample_rate, 48_000, "{ext}");
            assert_eq!(song.channels.len(), 2, "{ext}");
            // Lossy codecs may pad a few ms; lossless ones must be exact.
            let frames = song.frames() as i64;
            let tolerance = if ext == "aiff" || ext == "flac" { 0 } else { 3_000 };
            assert!((frames - 48_000).abs() <= tolerance, "{ext}: {frames} frames");
        }
    }
}
