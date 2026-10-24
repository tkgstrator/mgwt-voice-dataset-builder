use mgwt_voice_dataset_builder::audio::{AudioErrorKind, CanonicalWavSink};
use sha2::{Digest, Sha256};
use std::io::Write;

fn chunk(id: &[u8; 4], body: &[u8], pad: u8) -> Vec<u8> {
    let mut result = id.to_vec();
    result.extend_from_slice(&(body.len() as u32).to_le_bytes());
    result.extend_from_slice(body);
    if body.len() % 2 == 1 {
        result.push(pad);
    }
    result
}
fn pcm_fmt(channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let align = channels * (bits / 8);
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    fmt
}
fn wave(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = b"WAVE".to_vec();
    for chunk in chunks {
        body.extend_from_slice(chunk);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}
fn canonical(channels: u16, rate: u32, bits: u16, pcm: &[u8]) -> Vec<u8> {
    let out = wave(&[
        chunk(b"fmt ", &pcm_fmt(channels, rate, bits), 0),
        chunk(b"data", pcm, 0),
    ]);
    assert_eq!(out.len(), 44 + pcm.len() + pcm.len() % 2);
    out
}
fn run(
    source: &[u8],
    max: u64,
    fragment: usize,
) -> Result<
    (
        Vec<u8>,
        mgwt_voice_dataset_builder::audio::CanonicalWavReceipt,
    ),
    AudioErrorKind,
> {
    let mut out = Vec::new();
    let mut sink = CanonicalWavSink::new(&mut out, max);
    for slice in source.chunks(fragment) {
        if sink.write_all(slice).is_err() {
            return Err(sink.failure_kind().unwrap());
        }
    }
    let receipt = sink.finish().map_err(|error| error.kind())?;
    Ok((out, receipt))
}
#[test]
fn fragmented_riff_with_ancillary_chunks_becomes_exact_canonical_wav() {
    let pcm = [0x11, 0x22, 0x33];
    let input = wave(&[
        chunk(b"JUNK", &[1, 2, 3], 0x91),
        chunk(b"fmt ", &pcm_fmt(1, 8000, 8), 0),
        chunk(b"LIST", &[4, 5], 0),
        chunk(b"data", &pcm, 0x7f),
        chunk(b"fact", &[9], 0xee),
    ]);
    let expected = canonical(1, 8000, 8, &pcm);
    for fragment in [1, 2, 7, input.len()] {
        let (actual, receipt) = run(&input, 1024, fragment).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(receipt.wav_bytes, 48);
        assert_eq!(receipt.wav_sha256, hex::encode(Sha256::digest(&expected)));
        assert_eq!(
            (
                receipt.channels,
                receipt.sample_rate,
                receipt.bits_per_sample,
                receipt.frame_count
            ),
            (1, 8000, 8, 3)
        );
    }
}
#[test]
fn rejects_malformed_and_truncated_riff_as_clip_local() {
    let pcm = [1, 2];
    let normal = canonical(1, 8000, 8, &pcm);
    for bad in [
        normal[..normal.len() - 1].to_vec(),
        {
            let mut x = normal.clone();
            x[0] = b'X';
            x
        },
        {
            let mut x = normal.clone();
            x[4..8].copy_from_slice(&30u32.to_le_bytes());
            x
        },
        wave(&[chunk(b"fmt ", &pcm_fmt(1, 8000, 8), 0)]),
    ] {
        assert_eq!(run(&bad, 1024, 1).unwrap_err(), AudioErrorKind::Malformed);
    }
}
#[test]
fn rejects_duplicate_fmt_and_data_and_compressed_layout() {
    let fmt = chunk(b"fmt ", &pcm_fmt(1, 8000, 8), 0);
    let data = chunk(b"data", &[1], 0);
    for bad in [
        wave(&[fmt.clone(), fmt.clone(), data.clone()]),
        wave(&[fmt.clone(), data.clone(), data.clone()]),
        {
            let mut compressed = pcm_fmt(1, 8000, 8);
            compressed[..2].copy_from_slice(&3u16.to_le_bytes());
            wave(&[chunk(b"fmt ", &compressed, 0), data.clone()])
        },
        wave(&[data.clone(), fmt.clone()]),
    ] {
        assert_eq!(run(&bad, 1024, 3).unwrap_err(), AudioErrorKind::Malformed);
    }
}
#[test]
fn output_size_cap_includes_canonical_odd_padding() {
    let source = canonical(1, 8000, 8, &[1]);
    assert_eq!(run(&source, 45, 1).unwrap_err(), AudioErrorKind::Limit);
    assert_eq!(run(&source, 46, 1).unwrap().1.wav_bytes, 46);
}
#[test]
fn validates_pcm_rate_and_frame_alignment() {
    let mut fmt = pcm_fmt(2, 8000, 16);
    fmt[8..12].copy_from_slice(&123u32.to_le_bytes());
    let bad_rate = wave(&[chunk(b"fmt ", &fmt, 0), chunk(b"data", &[0; 4], 0)]);
    let bad_frames = wave(&[
        chunk(b"fmt ", &pcm_fmt(2, 8000, 16), 0),
        chunk(b"data", &[0; 3], 0),
    ]);
    for bad in [&bad_rate, &bad_frames] {
        assert_eq!(run(bad, 1024, 4).unwrap_err(), AudioErrorKind::Malformed);
    }
}
#[test]
fn accepts_zero_length_ancillary_chunk_at_riff_end() {
    let source = wave(&[
        chunk(b"fmt ", &pcm_fmt(1, 8000, 8), 0),
        chunk(b"data", &[7], 0),
        chunk(b"JUNK", &[], 0),
    ]);
    let (actual, receipt) = run(&source, 1024, source.len()).unwrap();
    assert_eq!(actual, canonical(1, 8000, 8, &[7]));
    assert_eq!(receipt.frame_count, 1);
}
#[test]
fn preserves_pcm_for_all_supported_bit_depths() {
    for (bits, pcm, expected_frames) in [
        (8, vec![1, 2], 2),
        (16, vec![1, 2], 1),
        (24, vec![1, 2, 3], 1),
        (32, vec![1, 2, 3, 4], 1),
    ] {
        let source = wave(&[
            chunk(b"fmt ", &pcm_fmt(1, 8000, bits), 0),
            chunk(b"data", &pcm, 0),
        ]);
        let (actual, receipt) = run(&source, 1024, 2).unwrap();
        assert_eq!(actual, canonical(1, 8000, bits, &pcm));
        assert_eq!(receipt.frame_count, expected_frames);
    }
}
#[test]
fn classifies_destination_failure_as_fatal_io() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let source = canonical(1, 8000, 8, &[1]);
    let mut writer = FailingWriter;
    let mut sink = CanonicalWavSink::new(&mut writer, 1024);
    assert!(sink.write_all(&source).is_err());
    assert_eq!(sink.failure_kind(), Some(AudioErrorKind::Io));
    assert_eq!(sink.finish().unwrap_err().kind(), AudioErrorKind::Io);
}
