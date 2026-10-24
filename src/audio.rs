//! Bounded streaming RIFF/WAVE PCM canonicalizer.
//! The caller must use a staged destination because a late RIFF error can follow emitted PCM.
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::{self, Write},
};
const HARD_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioErrorKind {
    Malformed,
    Limit,
    Io,
}
#[derive(Debug)]
pub enum AudioError {
    Malformed(&'static str),
    Limit(&'static str),
    Io(io::Error),
}
impl AudioError {
    pub fn kind(&self) -> AudioErrorKind {
        match self {
            Self::Malformed(_) => AudioErrorKind::Malformed,
            Self::Limit(_) => AudioErrorKind::Limit,
            Self::Io(_) => AudioErrorKind::Io,
        }
    }
}
impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(s) => write!(f, "malformed WAV: {s}"),
            Self::Limit(s) => write!(f, "WAV limit: {s}"),
            Self::Io(e) => write!(f, "WAV output I/O: {e}"),
        }
    }
}
impl std::error::Error for AudioError {}
#[derive(Debug, Clone)]
pub struct CanonicalWavReceipt {
    pub wav_bytes: u64,
    pub wav_sha256: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub frame_count: u64,
    pub peak_live_buffer_bytes: usize,
}
#[derive(Clone, Copy)]
struct Format {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    block_align: u16,
}
enum State {
    Riff([u8; 12], usize),
    Header([u8; 8], usize),
    Fmt {
        left: u32,
        pad: bool,
        first: [u8; 16],
        len: usize,
    },
    Skip {
        left: u32,
        pad: bool,
    },
    Data {
        left: u32,
        pad: bool,
    },
    Pad {
        data: bool,
    },
    Poison,
}
fn header_state() -> State {
    State::Header([0; 8], 0)
}
pub struct CanonicalWavSink<'a> {
    output: &'a mut dyn Write,
    maximum: u64,
    source_bytes: u64,
    riff_total: Option<u64>,
    written: u64,
    hasher: Sha256,
    state: State,
    format: Option<Format>,
    fmt_seen: bool,
    data_seen: bool,
    data_bytes: u32,
    peak_live_buffer_bytes: usize,
    failure: Option<AudioError>,
}
impl<'a> CanonicalWavSink<'a> {
    pub fn new(output: &'a mut dyn Write, maximum_wav_bytes: u64) -> Self {
        Self {
            output,
            maximum: maximum_wav_bytes.min(HARD_LIMIT),
            source_bytes: 0,
            riff_total: None,
            written: 0,
            hasher: Sha256::new(),
            state: State::Riff([0; 12], 0),
            format: None,
            fmt_seen: false,
            data_seen: false,
            data_bytes: 0,
            peak_live_buffer_bytes: 80,
            failure: None,
        }
    }
    pub fn failure_kind(&self) -> Option<AudioErrorKind> {
        self.failure.as_ref().map(AudioError::kind)
    }
    pub fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    pub fn finish(self) -> Result<CanonicalWavReceipt, AudioError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.riff_total != Some(self.source_bytes) || !matches!(self.state, State::Header(_, 0))
        {
            return Err(AudioError::Malformed(
                "RIFF is truncated or ends inside a chunk",
            ));
        }
        if !self.fmt_seen || !self.data_seen {
            return Err(AudioError::Malformed("fmt or data chunk is missing"));
        }
        let format = self.format.expect("validated fmt chunk");
        Ok(CanonicalWavReceipt {
            wav_bytes: self.written,
            wav_sha256: hex::encode(self.hasher.finalize()),
            channels: format.channels,
            sample_rate: format.sample_rate,
            bits_per_sample: format.bits_per_sample,
            frame_count: u64::from(self.data_bytes) / u64::from(format.block_align),
            peak_live_buffer_bytes: self.peak_live_buffer_bytes,
        })
    }
    fn advance(&mut self, n: usize) -> Result<(), AudioError> {
        let next = self
            .source_bytes
            .checked_add(n as u64)
            .ok_or(AudioError::Limit("source byte count overflow"))?;
        if next > HARD_LIMIT {
            return Err(AudioError::Limit("source RIFF exceeds hard limit"));
        }
        if self.riff_total.is_some_and(|total| next > total) {
            return Err(AudioError::Malformed("bytes after declared RIFF end"));
        }
        self.source_bytes = next;
        Ok(())
    }
    fn emit(&mut self, bytes: &[u8]) -> Result<(), AudioError> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or(AudioError::Limit("output size overflow"))?;
        if next > self.maximum {
            return Err(AudioError::Limit("canonical WAV exceeds configured cap"));
        }
        self.output.write_all(bytes).map_err(AudioError::Io)?;
        self.hasher.update(bytes);
        self.written = next;
        Ok(())
    }
    fn parse_fmt(&mut self, b: &[u8; 16]) -> Result<(), AudioError> {
        let w = |i| u16::from_le_bytes(b[i..i + 2].try_into().unwrap());
        let d = |i| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let (tag, channels, rate, byte_rate, align, bits) = (w(0), w(2), d(4), d(8), w(12), w(14));
        if tag != 1 || channels == 0 || rate == 0 || !matches!(bits, 8 | 16 | 24 | 32) {
            return Err(AudioError::Malformed("unsupported PCM format"));
        }
        let expected_align = u32::from(channels) * u32::from(bits / 8);
        let expected_rate = u64::from(rate) * u64::from(expected_align);
        if expected_align == 0
            || expected_align > u16::MAX as u32
            || u32::from(align) != expected_align
            || expected_rate > u32::MAX as u64
            || u64::from(byte_rate) != expected_rate
        {
            return Err(AudioError::Malformed(
                "PCM block align or byte rate is invalid",
            ));
        }
        self.format = Some(Format {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            block_align: align,
        });
        Ok(())
    }
    fn start_data(&mut self, size: u32, pad: bool) -> Result<State, AudioError> {
        if self.data_seen {
            return Err(AudioError::Malformed("duplicate data chunk"));
        }
        let format = self
            .format
            .ok_or(AudioError::Malformed("data precedes fmt chunk"))?;
        if size == 0 || !size.is_multiple_of(u32::from(format.block_align)) {
            return Err(AudioError::Malformed("data has no complete PCM frames"));
        }
        let total = 44 + u64::from(size) + u64::from(pad);
        if total > self.maximum {
            return Err(AudioError::Limit("canonical WAV exceeds configured cap"));
        }
        let mut h = [0u8; 44];
        h[..4].copy_from_slice(b"RIFF");
        h[4..8].copy_from_slice(&((total - 8) as u32).to_le_bytes());
        h[8..16].copy_from_slice(b"WAVEfmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        h[20..22].copy_from_slice(&1u16.to_le_bytes());
        h[22..24].copy_from_slice(&format.channels.to_le_bytes());
        h[24..28].copy_from_slice(&format.sample_rate.to_le_bytes());
        h[28..32]
            .copy_from_slice(&(format.sample_rate * u32::from(format.block_align)).to_le_bytes());
        h[32..34].copy_from_slice(&format.block_align.to_le_bytes());
        h[34..36].copy_from_slice(&format.bits_per_sample.to_le_bytes());
        h[36..40].copy_from_slice(b"data");
        h[40..44].copy_from_slice(&size.to_le_bytes());
        self.emit(&h)?;
        self.data_seen = true;
        self.data_bytes = size;
        Ok(State::Data { left: size, pad })
    }
    fn consume(&mut self, input: &[u8]) -> Result<(), AudioError> {
        let mut pos = 0;
        while pos < input.len() {
            let state = std::mem::replace(&mut self.state, State::Poison);
            self.state = match state {
                State::Riff(mut b, mut len) => {
                    let n = (12 - len).min(input.len() - pos);
                    self.advance(n)?;
                    b[len..len + n].copy_from_slice(&input[pos..pos + n]);
                    len += n;
                    pos += n;
                    if len < 12 {
                        State::Riff(b, len)
                    } else {
                        if &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
                            return Err(AudioError::Malformed("not a RIFF/WAVE stream"));
                        }
                        let size = u32::from_le_bytes(b[4..8].try_into().unwrap());
                        if size < 4 {
                            return Err(AudioError::Malformed("RIFF size excludes WAVE tag"));
                        }
                        let total = 8 + u64::from(size);
                        if total > HARD_LIMIT {
                            return Err(AudioError::Limit("source RIFF exceeds hard limit"));
                        }
                        self.riff_total = Some(total);
                        header_state()
                    }
                }
                State::Header(mut b, mut len) => {
                    let n = (8 - len).min(input.len() - pos);
                    self.advance(n)?;
                    b[len..len + n].copy_from_slice(&input[pos..pos + n]);
                    len += n;
                    pos += n;
                    if len < 8 {
                        State::Header(b, len)
                    } else {
                        let size = u32::from_le_bytes(b[4..8].try_into().unwrap());
                        let pad = size & 1 == 1;
                        if u64::from(size) + u64::from(pad)
                            > self.riff_total.unwrap() - self.source_bytes
                        {
                            return Err(AudioError::Malformed(
                                "chunk extends beyond RIFF boundary",
                            ));
                        }
                        match &b[..4] {
                            b"fmt " => {
                                if self.fmt_seen {
                                    return Err(AudioError::Malformed("duplicate fmt chunk"));
                                }
                                if size < 16 {
                                    return Err(AudioError::Malformed(
                                        "PCM fmt chunk is shorter than 16 bytes",
                                    ));
                                }
                                self.fmt_seen = true;
                                State::Fmt {
                                    left: size,
                                    pad,
                                    first: [0; 16],
                                    len: 0,
                                }
                            }
                            b"data" => self.start_data(size, pad)?,
                            _ if size == 0 => header_state(),
                            _ => State::Skip { left: size, pad },
                        }
                    }
                }
                State::Fmt {
                    mut left,
                    pad,
                    mut first,
                    mut len,
                } => {
                    let n = (left as usize).min(input.len() - pos);
                    self.advance(n)?;
                    let copy = (16 - len).min(n);
                    first[len..len + copy].copy_from_slice(&input[pos..pos + copy]);
                    len += copy;
                    left -= n as u32;
                    pos += n;
                    if left == 0 {
                        self.parse_fmt(&first)?;
                        if pad {
                            State::Pad { data: false }
                        } else {
                            header_state()
                        }
                    } else {
                        State::Fmt {
                            left,
                            pad,
                            first,
                            len,
                        }
                    }
                }
                State::Skip { mut left, pad } => {
                    let n = (left as usize).min(input.len() - pos);
                    self.advance(n)?;
                    pos += n;
                    left -= n as u32;
                    if left == 0 {
                        if pad {
                            State::Pad { data: false }
                        } else {
                            header_state()
                        }
                    } else {
                        State::Skip { left, pad }
                    }
                }
                State::Data { mut left, pad } => {
                    let n = (left as usize).min(input.len() - pos);
                    self.advance(n)?;
                    self.emit(&input[pos..pos + n])?;
                    pos += n;
                    left -= n as u32;
                    if left == 0 {
                        if pad {
                            State::Pad { data: true }
                        } else {
                            header_state()
                        }
                    } else {
                        State::Data { left, pad }
                    }
                }
                State::Pad { data } => {
                    self.advance(1)?;
                    pos += 1;
                    if data {
                        self.emit(&[0])?;
                    }
                    header_state()
                }
                State::Poison => return Err(AudioError::Malformed("invalid parser state")),
            };
        }
        Ok(())
    }
}
impl Write for CanonicalWavSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(error) = &self.failure {
            return Err(io::Error::other(error.to_string()));
        }
        self.peak_live_buffer_bytes = self
            .peak_live_buffer_bytes
            .max(80usize.saturating_add(bytes.len()));
        if let Err(error) = self.consume(bytes) {
            let message = error.to_string();
            self.failure = Some(error);
            return Err(io::Error::other(message));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(error) = &self.failure {
            return Err(io::Error::other(error.to_string()));
        }
        if let Err(error) = self.output.flush() {
            let message = error.to_string();
            self.failure = Some(AudioError::Io(error));
            return Err(io::Error::other(message));
        }
        Ok(())
    }
}
