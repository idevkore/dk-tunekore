//! A streaming WAV writer for exports: stereo, 16-bit (dithered) or 24-bit
//! integer, or 32-bit float, written block by block as a render runs.
//!
//! A file is written next to where it goes, as `<name>.wav.part`, and only
//! [`Writer::finish`] cuts it to its length and fills in its header. The
//! export moves every finished part into place once all are done; a writer
//! dropped unfinished (a cancel, a failure) removes its part, so nothing half
//! written is ever left behind.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CHANNELS: u16 = 2;
/// Written out in pieces of about this many bytes.
const BUFFER: usize = 256 * 1024;

/// How samples are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// 16-bit integer, with triangular (TPDF) dither of one step.
    Pcm16,
    /// 24-bit integer.
    #[default]
    Pcm24,
    /// 32-bit float: nothing above full scale is clipped.
    Float32,
}

impl Format {
    fn bytes(self) -> u16 {
        match self {
            Format::Pcm16 => 2,
            Format::Pcm24 => 3,
            Format::Float32 => 4,
        }
    }

    /// 1 is integer PCM, 3 is IEEE float.
    fn tag(self) -> u16 {
        if self == Format::Float32 { 3 } else { 1 }
    }

    /// The header's length: float files carry `cbSize` and a `fact` chunk, as
    /// the format asks of anything that isn't integer PCM.
    fn header(self) -> u64 {
        if self == Format::Float32 { 58 } else { 44 }
    }
}

/// Where a part file goes while it's being written.
pub fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    path.with_file_name(name)
}

/// A small, fast random source for the dither.
struct Dither(u32);

impl Dither {
    /// Uniform in 0..1.
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Triangular in -1..1 (one 16-bit step either way).
    fn tpdf(&mut self) -> f32 {
        self.next() - self.next()
    }
}

/// One file being written. See the module notes.
pub struct Writer {
    path: PathBuf,
    part: PathBuf,
    file: Option<BufWriter<File>>,
    format: Format,
    rate: u32,
    frames: u64,
    /// Samples beyond full scale that an integer format had to clip.
    clipped: u64,
    dither: Dither,
    bytes: Vec<u8>,
}

impl Writer {
    /// Starts `path` (as its part file) at `rate`.
    pub fn create(path: &Path, rate: u32, format: Format) -> io::Result<Self> {
        let part = part_path(path);
        let mut file = BufWriter::with_capacity(BUFFER, File::create(&part)?);
        // The header, with the lengths filled in by `finish`.
        let header = header(format, rate, 0);
        if let Err(e) = file.write_all(&header) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        // Each file its own dither (FNV-1a of its path), so stems summed elsewhere don't add theirs up in step.
        let seed = path.as_os_str().as_encoded_bytes().iter().fold(0x811C_9DC5u32, |h, &b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
        Ok(Self { path: path.to_path_buf(), part, file: Some(file), format, rate, frames: 0, clipped: 0, dither: Dither(seed | 1), bytes: Vec::new() })
    }

    /// Where the file goes once finished.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn clipped(&self) -> u64 {
        self.clipped
    }

    /// Adds frames, left and right (the same length).
    pub fn write(&mut self, left: &[f32], right: &[f32]) -> io::Result<()> {
        let n = left.len().min(right.len());
        let per = usize::from(self.format.bytes()) * 2;
        let most = (u64::from(u32::MAX) - self.format.header()) / per as u64;
        if self.frames + n as u64 > most {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too long for a WAV file (4 GB)"));
        }
        self.bytes.clear();
        self.bytes.reserve(n * per);
        for i in 0..n {
            for x in [left[i], right[i]] {
                let x = if x.is_finite() { x } else { 0.0 };
                if x.abs() > 1.0 && self.format != Format::Float32 {
                    self.clipped += 1;
                }
                match self.format {
                    Format::Pcm16 => {
                        let v = (x * 32_767.0 + self.dither.tpdf()).round().clamp(-32_768.0, 32_767.0) as i16;
                        self.bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    Format::Pcm24 => {
                        let v = (x.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                        self.bytes.extend_from_slice(&v.to_le_bytes()[..3]);
                    }
                    Format::Float32 => self.bytes.extend_from_slice(&x.to_le_bytes()),
                }
            }
        }
        self.frames += n as u64;
        match &mut self.file {
            Some(f) => f.write_all(&self.bytes),
            None => Err(io::Error::other("the file is closed")),
        }
    }

    /// Cuts the file to its first `frames` frames and fills in its header.
    /// Returns the part file, ready to move to [`path`](Self::path).
    pub fn finish(mut self, frames: u64) -> io::Result<PathBuf> {
        let mut file = self.file.take().ok_or_else(|| io::Error::other("the file is closed"))?.into_inner().map_err(|e| e.into_error())?;
        let frames = frames.min(self.frames);
        let data = frames * u64::from(self.format.bytes()) * u64::from(CHANNELS);
        file.set_len(self.format.header() + data)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header(self.format, self.rate, frames))?;
        file.sync_all()?;
        Ok(std::mem::take(&mut self.part))
    }
}

impl Drop for Writer {
    /// Unfinished (cancelled, failed, or `finish` failed): the part goes, so
    /// nothing half written stays.
    fn drop(&mut self) {
        if !self.part.as_os_str().is_empty() {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// The header for `frames` frames of `format` at `rate`.
fn header(format: Format, rate: u32, frames: u64) -> Vec<u8> {
    let bytes = format.bytes();
    let align = bytes * CHANNELS;
    let data = (frames * u64::from(align)) as u32;
    let mut h = Vec::with_capacity(format.header() as usize);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&((format.header() as u32 - 8) + data).to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    let float = format == Format::Float32;
    h.extend_from_slice(&(if float { 18u32 } else { 16 }).to_le_bytes());
    h.extend_from_slice(&format.tag().to_le_bytes());
    h.extend_from_slice(&CHANNELS.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    h.extend_from_slice(&align.to_le_bytes());
    h.extend_from_slice(&(bytes * 8).to_le_bytes());
    if float {
        h.extend_from_slice(&0u16.to_le_bytes()); // cbSize
        h.extend_from_slice(b"fact");
        h.extend_from_slice(&4u32.to_le_bytes());
        h.extend_from_slice(&(frames as u32).to_le_bytes());
    }
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    debug_assert_eq!(h.len() as u64, format.header());
    h
}
