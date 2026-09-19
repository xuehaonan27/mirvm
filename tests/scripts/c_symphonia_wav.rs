#!/usr/bin/env mirvm
---
[dependencies]
hound = "3"
# The feature set here is ["wav"], but that only enables symphonia-format-riff (container
# probing) and carries no decoder: after a successful probe, get_codecs().make() reports
# Unsupported("core (codec):unsupported codec") for PCM_S16LE, because symphonia-codec-pcm is
# not in the lockfile. The decode half of the loop therefore needs "pcm" as well
# (symphonia-codec-pcm, pure Rust with no C dependency); probing still comes from wav alone.
symphonia = { version = "0.5", default-features = false, features = ["wav", "pcm"] }
---
// hound 3 + symphonia 0.5 (wav container + pcm decode) audio loop differential. hound
// synthesizes at fixed parameters and writes a 16-bit PCM wav to a fixed temp_dir name
// (8 kHz mono, 2 s: 0-1 s a 440 Hz sine, 1-2 s a 200->2000 Hz linear chirp, constant
// amplitude); hound reads it back to cross-check; symphonia probes with a Hint + wav and
// prints the CodecParameters surface; all packets are decoded and compared sample by
// sample against the source (counts, first 24 values, whole-stream i16-bits FNV, mismatch
// count); a fresh open then seeks to 1.5 s (Accurate) and decodes the next packet; the
// metadata must report current = None (a PCM wav has no LIST INFO); and two bad-file error
// paths (junk bytes rejected by probe, a truncated file hitting EOF mid-decode).
// Deterministic: only counts, booleans and integers; no paths, time or addresses; empty stderr.
use std::f64::consts::PI;
use std::io::Cursor;
use std::path::PathBuf;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{CodecType, DecoderOptions, CODEC_TYPE_PCM_S16LE};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use symphonia::default::{get_codecs, get_probe};

/// Sample rate and total sample count (8 kHz × 2 s).
const SR: u32 = 8000;
const N: usize = 16000;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Bit-level anchor for a sample stream: every i16 as LE bytes fed to FNV-1a.
fn samples_fnv(s: &[i16]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &v in s {
        for b in v.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// Fixed-parameter synthesis: 0-1 s at 440 Hz sine (0.45), 1-2 s at 200->2000 Hz linear
/// chirp (0.30). The chirp phase integrates frequency (quadratic); f64 throughout, rounded to i16.
fn synth() -> Vec<i16> {
    let mut v = Vec::with_capacity(N);
    for i in 0..N {
        let t = i as f64 / SR as f64;
        let y = if i < 8000 {
            0.45 * (2.0 * PI * 440.0 * t).sin()
        } else {
            let tau = t - 1.0;
            let phase = 2.0 * PI * (200.0 * tau + 0.5 * 1800.0 * tau * tau);
            0.30 * phase.sin()
        };
        v.push((y * 32767.0).round() as i16);
    }
    v
}

fn spec16() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}

fn codec_name(t: CodecType) -> &'static str {
    match t {
        CODEC_TYPE_PCM_S16LE => "pcm_s16le",
        _ => "other",
    }
}

/// Interleaves the decoder's AudioBufferRef frame by frame into i16 and appends to out,
/// returning the frame count; a non-S16 buffer gives None (only S16 should appear here).
fn drain_i16(b: &AudioBufferRef<'_>, out: &mut Vec<i16>) -> Option<usize> {
    match b {
        AudioBufferRef::S16(buf) => {
            let nch = buf.spec().channels.count();
            let frames = buf.frames();
            for f in 0..frames {
                for ch in 0..nch {
                    out.push(buf.chan(ch)[f]);
                }
            }
            Some(frames)
        }
        _ => None,
    }
}

/// Open a stream and probe it (the Hint always carries the wav extension).
fn probe_mss(mss: MediaSourceStream) -> Result<symphonia::core::probe::ProbeResult, Error> {
    let mut hint = Hint::new();
    hint.with_extension("wav");
    get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )
}

fn main() {
    // ---- ⓪ Fixed-parameter synthesis (clear temp leftovers first, clean up at the end) ----
    let path: PathBuf = std::env::temp_dir().join("mirvm_corpus_symphonia_wav.wav");
    let _ = std::fs::remove_file(&path);

    let src = synth();
    println!("synth n={} fnv={:016x}", src.len(), samples_fnv(&src));
    println!("synth first24={:?}", &src[..24]);
    println!("synth tail8={:?}", &src[N - 8..]);
    println!(
        "synth min={} max={}",
        src.iter().copied().min().unwrap(),
        src.iter().copied().max().unwrap()
    );

    // ---- ① hound writes the wav (16-bit PCM, sample by sample) ----
    {
        let mut w = hound::WavWriter::create(&path, spec16()).unwrap();
        for &v in &src {
            w.write_sample(v).unwrap();
        }
        println!("hound write len={} dur={}", w.len(), w.duration());
        w.finalize().unwrap();
    }
    let file_bytes = std::fs::read(&path).unwrap();
    println!(
        "wav bytes len={} fnv={:016x}",
        file_bytes.len(),
        fnv1a(&file_bytes)
    );

    // ---- ② hound reads it back: spec fields + per-sample equality ----
    let mut rd = hound::WavReader::open(&path).unwrap();
    let sp = rd.spec();
    println!(
        "hound read ch={} sr={} bits={} int={} len={} dur={}",
        sp.channels,
        sp.sample_rate,
        sp.bits_per_sample,
        sp.sample_format == hound::SampleFormat::Int,
        rd.len(),
        rd.duration()
    );
    let back: Vec<i16> = rd.samples::<i16>().collect::<Result<_, _>>().unwrap();
    println!("hound roundtrip n={} eq={}", back.len(), back == src);

    // ---- ③ symphonia: probe the format + the CodecParameters surface ----
    let file = std::fs::File::open(&path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut probed = probe_mss(mss).unwrap();
    println!("tracks={}", probed.format.tracks().len());
    let track = probed.format.default_track().unwrap();
    let track_id = track.id;
    let cp = &track.codec_params;
    println!(
        "track id={} codec={} sr={:?} ch={:?} bits={:?} max_fpp={:?} fpb={:?} frames={:?}",
        track_id,
        codec_name(cp.codec),
        cp.sample_rate,
        cp.channels.map(|c| c.count()),
        cp.bits_per_sample,
        cp.max_frames_per_packet,
        cp.frames_per_block,
        cp.n_frames
    );

    // ---- ④ Decode all packets: per-sample comparison against the source + bit-level anchor ----
    let mut dec = get_codecs()
        .make(&track.codec_params, &DecoderOptions { verify: false })
        .unwrap();
    let mut got: Vec<i16> = Vec::with_capacity(N);
    let (mut packets, mut frames, mut payload_bytes) = (0u32, 0u64, 0u64);
    loop {
        let pkt = match probed.format.next_packet() {
            Ok(p) => p,
            Err(Error::ResetRequired) => panic!("reset required: unsupported"),
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("next_packet: {e}"),
        };
        if pkt.track_id() != track_id {
            continue;
        }
        payload_bytes += pkt.data.len() as u64;
        packets += 1;
        frames += pkt.dur() as u64;
        let decoded = dec.decode(&pkt).unwrap();
        let fr = drain_i16(&decoded, &mut got).expect("non-s16 buffer");
        assert_eq!(fr as u64, pkt.dur() as u64);
    }
    println!(
        "decode packets={} frames={} payload={} samples={}",
        packets,
        frames,
        payload_bytes,
        got.len()
    );
    let mismatches = src.iter().zip(got.iter()).filter(|(a, b)| a != b).count();
    println!(
        "cmp count_eq={} mismatch={} fnv={:016x}",
        src.len() == got.len(),
        mismatches,
        samples_fnv(&got)
    );
    if mismatches > 0 {
        let idx = src
            .iter()
            .zip(got.iter())
            .position(|(a, b)| a != b)
            .unwrap();
        println!("first mismatch at {idx}: src={} got={}", src[idx], got[idx]);
    }
    println!("decode first24={:?}", &got[..24]);

    // ---- ⑤ seek(Accurate, t=1.5 s) -> decode the next packet to check ----
    let file2 = std::fs::File::open(&path).unwrap();
    let mss2 = MediaSourceStream::new(Box::new(file2), MediaSourceStreamOptions::default());
    let mut probed2 = probe_mss(mss2).unwrap();
    let track2 = probed2.format.default_track().unwrap();
    let (tid2, cp2) = (track2.id, track2.codec_params.clone());
    let sought = probed2
        .format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::new(1, 0.5),
                track_id: Some(tid2),
            },
        )
        .unwrap();
    println!(
        "seek required_ts={} actual_ts={}",
        sought.required_ts, sought.actual_ts
    );
    let mut dec2 = get_codecs()
        .make(&cp2, &DecoderOptions { verify: false })
        .unwrap();
    let pkt = probed2.format.next_packet().unwrap();
    println!("post-seek pkt ts={} dur={}", pkt.ts(), pkt.dur());
    let mut seg: Vec<i16> = Vec::new();
    let decoded = dec2.decode(&pkt).unwrap();
    drain_i16(&decoded, &mut seg).expect("non-s16 buffer");
    let start = pkt.ts() as usize;
    let expect = &src[start..start + 4];
    println!(
        "post-seek first4={:?} want={:?} eq={}",
        &seg[..4],
        expect,
        seg[..4] == *expect
    );

    // ---- ⑥ Metadata surface: a PCM wav has no LIST INFO, so current must be None ----
    match probed2.format.metadata().current() {
        Some(rev) => println!(
            "metadata tags={} visuals={}",
            rev.tags().len(),
            rev.visuals().len()
        ),
        None => println!("metadata current=None"),
    }

    // ---- ⑦ Error paths, two of them ----
    // (a) Junk bytes: probe must reject them.
    let junk = Cursor::new(b"this is definitely not a RIFF/WAVE file.........".to_vec());
    match probe_mss(MediaSourceStream::new(Box::new(junk), Default::default())) {
        Ok(_) => println!("junk probe: unexpected ok"),
        Err(e) => println!("junk probe err: {e}"),
    }
    // (b) Truncated file (valid header kept, most of data cut): probe succeeds, decode hits EOF mid-stream.
    let cut_len = 128usize;
    let cut = Cursor::new(file_bytes[..cut_len].to_vec());
    let mut probed3 = probe_mss(MediaSourceStream::new(Box::new(cut), Default::default()))
        .expect("truncated: probe should succeed");
    let t3 = probed3.format.default_track().unwrap();
    let mut dec3 = get_codecs()
        .make(&t3.codec_params, &DecoderOptions { verify: false })
        .unwrap();
    let mut got3: Vec<i16> = Vec::new();
    println!("truncated result: {}", || -> String {
        loop {
            let pkt = match probed3.format.next_packet() {
                Ok(p) => p,
                Err(Error::ResetRequired) => return "reset-required".to_string(),
                Err(Error::IoError(e)) => {
                    return format!("io {:?} after {} samples: {}", e.kind(), got3.len(), e)
                }
                Err(e) => return format!("other after {} samples: {e}", got3.len()),
            };
            let d = dec3.decode(&pkt).unwrap();
            drain_i16(&d, &mut got3).expect("non-s16 buffer");
        }
    }());

    // ---- ⑧ Clean up the temporary file ----
    std::fs::remove_file(&path).unwrap();
    println!("cleanup exists={}", path.exists());
}
