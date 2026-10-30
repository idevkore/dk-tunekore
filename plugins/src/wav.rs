//! A minimal WAV writer (32-bit float), for offline renders and test output.

use std::io::{self, Write};
use std::path::Path;

/// Writes `channels` (all the same length) as an interleaved 32-bit float WAV file.
pub fn write_f32(path: &Path, sample_rate: u32, channels: &[Vec<f32>]) -> io::Result<()> {
    let n_ch = channels.len().max(1);
    let frames = channels.first().map_or(0, Vec::len);
    if channels.iter().any(|c| c.len() != frames) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "channels differ in length"));
    }
    let data_bytes = u32::try_from(frames * n_ch * 4).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too long for a WAV file"))?;
    let mut out = io::BufWriter::new(std::fs::File::create(path)?);
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_bytes).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&3u16.to_le_bytes())?; // IEEE float
    out.write_all(&(n_ch as u16).to_le_bytes())?;
    out.write_all(&sample_rate.to_le_bytes())?;
    out.write_all(&(sample_rate * n_ch as u32 * 4).to_le_bytes())?;
    out.write_all(&(n_ch as u16 * 4).to_le_bytes())?;
    out.write_all(&32u16.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&data_bytes.to_le_bytes())?;
    for i in 0..frames {
        for c in channels {
            out.write_all(&c[i].to_le_bytes())?;
        }
    }
    out.flush()
}

/// Reads back a file written by [`write_f32`]: (sample rate, channels).
pub fn read_f32(path: &Path) -> io::Result<(u32, Vec<Vec<f32>>)> {
    let bytes = std::fs::read(path)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "not a 32-bit float WAV file");
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..16] != b"WAVEfmt " || bytes[20..22] != 3u16.to_le_bytes() {
        return Err(bad());
    }
    let n_ch = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let data = &bytes[44..];
    if n_ch == 0 || data.len() % (4 * n_ch) != 0 {
        return Err(bad());
    }
    let mut channels = vec![Vec::with_capacity(data.len() / 4 / n_ch); n_ch];
    for (i, s) in data.as_chunks::<4>().0.iter().enumerate() {
        channels[i % n_ch].push(f32::from_le_bytes(*s));
    }
    Ok((rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_reads_back() {
        let path = std::env::temp_dir().join(format!("tunekore-wav-test-{}.wav", std::process::id()));
        let ch = vec![vec![0.0, 0.5, -0.5], vec![1.0, 0.25, -1.0]];
        write_f32(&path, 44_100, &ch).unwrap();
        assert_eq!(read_f32(&path).unwrap(), (44_100, ch));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn uneven_channels_are_refused() {
        let path = std::env::temp_dir().join("tunekore-wav-uneven.wav");
        assert!(write_f32(&path, 48_000, &[vec![0.0], vec![0.0, 1.0]]).is_err());
    }
}
