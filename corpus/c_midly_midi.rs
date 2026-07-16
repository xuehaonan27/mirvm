#!/usr/bin/env mirvm
---
[dependencies]
# 任务规格写 midly = "0.10"，但 crates.io sparse index 与 docs.rs 一致表明
# midly 最新版只有 0.5.3（2023-01 发布）——"0.10" 不存在，钉死真实最新版。
# default-features 含 parallel（rayon 多线程写出），对差分无意义且引入并行
# 维度；只留 std（提供 write_std / save）。此组合下 midly 零依赖纯 Rust。
midly = { version = "=0.5.3", default-features = false, features = ["std"] }
---
// midly 0.5.3：SMF 全内存闭环差分。构建 format-1 三轨文件（conductor 元事件轨
// + 多 channel 力度谱系 note on/off 轨 + SysEx/Escape/杂项 meta 轨）→ write_std
// 写字节（len+FNV 锚定）→ Smf::parse 逐事件确定序回放（类型/abs tick/delta/
// channel/键/力度）→ 再写出验证 roundtrip 逐字节不变量。
// 覆盖：格式 0/1/2 × Metrical/Timecode 两种 Timing；u28 varlen delta 边界
// （0/1/127/128/8191/8192/16383/16384/2^21±1/u28::MAX）；受限整数 try_from
// 边界（u4/u7/u14/u15/u24/u28）；RIFF/RMID 解包；错误路径（空输入/坏魔数/
// 非 RMID 的 RIFF/非法格式号 3/头后截断/轨中截断/尾截断——注意 midly 默认
// lenient 模式，头后截断会以 0 轨解析成功，属规格行为两端正面对拍）；
// live::LiveEvent 原始字节解析（Midi/Realtime/SysEx/空输入）；SmfBytemap
// 事件-字节映射。
// 确定性：Vec 顺序打印，无 HashMap 迭代/地址/时间/线程序；文本走
// from_utf8_lossy；二进制只打 len+FNV；不打印浮点。
use midly::live::LiveEvent;
use midly::num::{u14, u15, u24, u28, u4, u7};
use midly::{
    Fps, Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, SmfBytemap, SmpteTime, Timing,
    TrackEvent, TrackEventKind,
};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn ev(delta: u32, kind: TrackEventKind<'static>) -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::new(delta),
        kind,
    }
}

fn midi(delta: u32, channel: u8, message: MidiMessage) -> TrackEvent<'static> {
    ev(
        delta,
        TrackEventKind::Midi {
            channel: u4::new(channel),
            message,
        },
    )
}

fn note_on(delta: u32, channel: u8, key: u8, vel: u8) -> TrackEvent<'static> {
    midi(
        delta,
        channel,
        MidiMessage::NoteOn {
            key: u7::new(key),
            vel: u7::new(vel),
        },
    )
}

fn note_off(delta: u32, channel: u8, key: u8, vel: u8) -> TrackEvent<'static> {
    midi(
        delta,
        channel,
        MidiMessage::NoteOff {
            key: u7::new(key),
            vel: u7::new(vel),
        },
    )
}

/// 把单个事件格式化成确定序文本行（abs 为调用方累加的绝对 tick）。
fn describe_event(abs: u32, ev: &TrackEvent) -> String {
    let head = format!("abs={abs} dt={}", ev.delta.as_int());
    match &ev.kind {
        TrackEventKind::Midi { channel, message } => {
            let ch = channel.as_int();
            let msg = match message {
                MidiMessage::NoteOff { key, vel } => {
                    format!("noteoff key={} vel={}", key.as_int(), vel.as_int())
                }
                MidiMessage::NoteOn { key, vel } => {
                    format!("noteon key={} vel={}", key.as_int(), vel.as_int())
                }
                MidiMessage::Aftertouch { key, vel } => {
                    format!("aftertouch key={} vel={}", key.as_int(), vel.as_int())
                }
                MidiMessage::Controller { controller, value } => {
                    format!("controller cc={} val={}", controller.as_int(), value.as_int())
                }
                MidiMessage::ProgramChange { program } => {
                    format!("program prog={}", program.as_int())
                }
                MidiMessage::ChannelAftertouch { vel } => {
                    format!("chanafter vel={}", vel.as_int())
                }
                MidiMessage::PitchBend { bend } => format!("pitchbend val={}", bend.as_int()),
            };
            format!("{head} midi ch={ch} {msg}")
        }
        TrackEventKind::SysEx(data) => format!("{head} sysex len={} fnv={:016x}", data.len(), fnv1a(data)),
        TrackEventKind::Escape(data) => {
            format!("{head} escape len={} fnv={:016x}", data.len(), fnv1a(data))
        }
        TrackEventKind::Meta(msg) => {
            let m = match msg {
                MetaMessage::TrackNumber(n) => format!("tracknumber {n:?}"),
                MetaMessage::Text(s) => format!("text {:?}", String::from_utf8_lossy(s)),
                MetaMessage::Copyright(s) => format!("copyright {:?}", String::from_utf8_lossy(s)),
                MetaMessage::TrackName(s) => format!("trackname {:?}", String::from_utf8_lossy(s)),
                MetaMessage::InstrumentName(s) => {
                    format!("instrument {:?}", String::from_utf8_lossy(s))
                }
                MetaMessage::Lyric(s) => format!("lyric {:?}", String::from_utf8_lossy(s)),
                MetaMessage::Marker(s) => format!("marker {:?}", String::from_utf8_lossy(s)),
                MetaMessage::CuePoint(s) => format!("cuepoint {:?}", String::from_utf8_lossy(s)),
                MetaMessage::ProgramName(s) => format!("programname {:?}", String::from_utf8_lossy(s)),
                MetaMessage::DeviceName(s) => format!("devicename {:?}", String::from_utf8_lossy(s)),
                MetaMessage::MidiChannel(c) => format!("metachannel {}", c.as_int()),
                MetaMessage::MidiPort(p) => format!("metaport {}", p.as_int()),
                MetaMessage::EndOfTrack => "endoftrack".to_string(),
                MetaMessage::Tempo(t) => format!("tempo us-per-beat={}", t.as_int()),
                MetaMessage::SmpteOffset(t) => format!(
                    "smpteoffset {}:{}:{}:{}:{}@{}",
                    t.hour(),
                    t.minute(),
                    t.second(),
                    t.frame(),
                    t.subframe(),
                    t.fps().as_int()
                ),
                MetaMessage::TimeSignature(n, d, c, b) => {
                    format!("timesig {n}/{d} clocks={c} 32nd={b}")
                }
                MetaMessage::KeySignature(sf, minor) => format!("keysig sf={sf} minor={minor}"),
                MetaMessage::SequencerSpecific(d) => {
                    format!("seqspecific len={} fnv={:016x}", d.len(), fnv1a(d))
                }
                MetaMessage::Unknown(id, d) => {
                    format!("unknown id=0x{id:02x} len={} fnv={:016x}", d.len(), fnv1a(d))
                }
            };
            format!("{head} meta {m}")
        }
    }
}

fn dump_smf(label: &str, smf: &Smf) {
    let fmt_code = match smf.header.format {
        Format::SingleTrack => 0,
        Format::Parallel => 1,
        Format::Sequential => 2,
    };
    let timing = match &smf.header.timing {
        Timing::Metrical(t) => format!("metrical:{}", t.as_int()),
        Timing::Timecode(fps, sub) => format!("timecode:{}:{}", fps.as_int(), sub),
    };
    println!(
        "{label} header fmt={fmt_code} timing={timing} ntracks={}",
        smf.tracks.len()
    );
    for (ti, track) in smf.tracks.iter().enumerate() {
        println!("{label} -- track {ti} events={}", track.len());
        let mut abs = 0u32;
        for (ei, event) in track.iter().enumerate() {
            abs += event.delta.as_int();
            println!("{label} t{ti}[{ei}] {}", describe_event(abs, event));
        }
    }
}

/// ① 构建 format-1 三轨 SMF 并写出字节。
fn build_main_smf() -> Smf<'static> {
    // 轨 0：conductor 元事件轨
    let track0 = vec![
        ev(0, TrackEventKind::Meta(MetaMessage::TrackName(b"mirvm differential suite"))),
        ev(0, TrackEventKind::Meta(MetaMessage::Copyright(b"(C) 2026 mirvm corpus"))),
        ev(0, TrackEventKind::Meta(MetaMessage::Text("unicode 文本 🎹 第二行\t制表".as_bytes()))),
        ev(0, TrackEventKind::Meta(MetaMessage::TimeSignature(6, 8, 24, 8))),
        ev(0, TrackEventKind::Meta(MetaMessage::KeySignature(-3, true))),
        ev(0, TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000)))),
        ev(
            0,
            TrackEventKind::Meta(MetaMessage::SmpteOffset(
                SmpteTime::new(1, 2, 3, 4, 5, Fps::Fps25).unwrap(),
            )),
        ),
        ev(480, TrackEventKind::Meta(MetaMessage::Tempo(u24::new(250_000)))),
        // u24 最大值边界：16777215 us/beat
        ev(480, TrackEventKind::Meta(MetaMessage::Tempo(u24::new(0xFF_FFFF)))),
        ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
    ];

    // 轨 1：多 channel 音符轨，力度谱系 + 控制器/弯音/触后
    let mut track1 = vec![
        ev(0, TrackEventKind::Meta(MetaMessage::TrackName(b"piano"))),
        midi(0, 0, MidiMessage::ProgramChange { program: u7::new(0) }),
        midi(0, 9, MidiMessage::ProgramChange { program: u7::new(40) }),
    ];
    // 力度谱系：9 档 on/off（off 带释放力度），key 逐档上移
    for (i, vel) in [1u8, 16, 32, 48, 64, 80, 96, 112, 127].iter().enumerate() {
        let key = 60 + i as u8;
        track1.push(note_on(24, 0, key, *vel));
        track1.push(note_off(24, 0, key, *vel));
    }
    // NoteOn vel=0（等价 NoteOff 的惯例路径）
    track1.push(note_on(0, 0, 60, 0));
    track1.push(note_off(96, 0, 60, 64));
    // 触后/通道触后/控制器/弯音（含 min/mid/max 与 from_int 夹取）
    track1.push(midi(0, 0, MidiMessage::Aftertouch { key: u7::new(64), vel: u7::new(90) }));
    track1.push(midi(0, 0, MidiMessage::ChannelAftertouch { vel: u7::new(77) }));
    track1.push(midi(0, 0, MidiMessage::Controller { controller: u7::new(7), value: u7::new(100) }));
    track1.push(midi(0, 0, MidiMessage::Controller { controller: u7::new(10), value: u7::new(32) }));
    track1.push(midi(0, 0, MidiMessage::PitchBend { bend: PitchBend::min_raw_value() }));
    track1.push(midi(12, 0, MidiMessage::PitchBend { bend: PitchBend::mid_raw_value() }));
    track1.push(midi(12, 0, MidiMessage::PitchBend { bend: PitchBend::max_raw_value() }));
    track1.push(midi(12, 0, MidiMessage::PitchBend { bend: PitchBend::from_int(-0x2000) }));
    // channel 9 打击乐
    track1.push(note_on(0, 9, 35, 120));
    track1.push(note_off(48, 9, 35, 100));
    track1.push(note_on(0, 9, 38, 110));
    track1.push(note_off(48, 9, 38, 90));
    track1.push(ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)));

    // 轨 2：SysEx / Escape / 杂项 meta
    let track2 = vec![
        ev(0, TrackEventKind::Meta(MetaMessage::TrackName(b"synth fx"))),
        ev(0, TrackEventKind::SysEx(&[0x43, 0x12, 0x00, 0x00, 0x7F, 0x01])),
        // 空 SysEx 边界
        ev(12, TrackEventKind::SysEx(&[])),
        ev(12, TrackEventKind::Escape(&[0xF3, 0x7F, 0x01])),
        ev(0, TrackEventKind::Meta(MetaMessage::SequencerSpecific(&[0x00, 0x41, 0x10, 0x42]))),
        ev(0, TrackEventKind::Meta(MetaMessage::TrackNumber(Some(2)))),
        ev(0, TrackEventKind::Meta(MetaMessage::MidiChannel(u4::new(5)))),
        ev(0, TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(3)))),
        ev(0, TrackEventKind::Meta(MetaMessage::Lyric("词 🎵 lyric".as_bytes()))),
        ev(0, TrackEventKind::Meta(MetaMessage::Marker(b"verse-1"))),
        ev(0, TrackEventKind::Meta(MetaMessage::CuePoint(b"cue-1"))),
        ev(0, TrackEventKind::Meta(MetaMessage::InstrumentName(b"lead synth"))),
        ev(0, TrackEventKind::Meta(MetaMessage::ProgramName(b"prog-A"))),
        ev(0, TrackEventKind::Meta(MetaMessage::DeviceName(b"dev-0"))),
        ev(0, TrackEventKind::Meta(MetaMessage::Unknown(0x7E, &[0xAA, 0xBB, 0xCC]))),
        ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
    ];

    Smf {
        header: Header::new(Format::Parallel, Timing::Metrical(u15::new(480))),
        tracks: vec![track0, track1, track2],
    }
}

/// ② 格式 0/1/2 × Timing 矩阵：小文件写出→解析→指纹。
fn format_matrix() {
    for (label, format, ntracks) in [
        ("fmt0", Format::SingleTrack, 1usize),
        ("fmt1", Format::Parallel, 2),
        ("fmt2", Format::Sequential, 2),
    ] {
        let mut smf = Smf::new(Header::new(format, Timing::Metrical(u15::new(96))));
        let names: [&[u8]; 2] = [b"alpha", b"beta"];
        for (t, name) in names.iter().enumerate().take(ntracks) {
            smf.tracks.push(vec![
                ev(0, TrackEventKind::Meta(MetaMessage::TrackName(name))),
                note_on(0, t as u8, 64, 100),
                note_off(96, t as u8, 64, 64),
                ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
            ]);
        }
        let mut bytes = Vec::new();
        smf.write_std(&mut bytes).unwrap();
        let back = Smf::parse(&bytes).unwrap();
        let events: usize = back.tracks.iter().map(|t| t.len()).sum();
        println!(
            "{label} len={} fnv={:016x} tracks={} events={}",
            bytes.len(),
            fnv1a(&bytes),
            back.tracks.len(),
            events
        );
    }
    // Timecode timing（SMPTE 29.97fps × 40 subframe）与 u15 最大值
    let mut tc = Smf::new(Header::new(
        Format::SingleTrack,
        Timing::Timecode(Fps::Fps29, 40),
    ));
    tc.tracks.push(vec![
        note_on(0, 0, 60, 90),
        note_off(40, 0, 60, 40),
        ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)),
    ]);
    let mut bytes = Vec::new();
    tc.write_std(&mut bytes).unwrap();
    let back = Smf::parse(&bytes).unwrap();
    match back.header.timing {
        Timing::Timecode(fps, sub) => {
            println!("timecode rt fps={} sub={} len={}", fps.as_int(), sub, bytes.len())
        }
        Timing::Metrical(_) => println!("timecode rt unexpected metrical"),
    }
    let mut mx = Smf::new(Header::new(
        Format::SingleTrack,
        Timing::Metrical(u15::new(0x7FFF)),
    ));
    mx.tracks
        .push(vec![ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack))]);
    let mut bytes = Vec::new();
    mx.write_std(&mut bytes).unwrap();
    match Smf::parse(&bytes).unwrap().header.timing {
        Timing::Metrical(t) => println!("metrical-max rt tpb={}", t.as_int()),
        Timing::Timecode(..) => println!("metrical-max rt unexpected timecode"),
    }
}

/// ③ varlen delta 边界：含 u28::MAX 的大 delta time 写出→解析逐值回放。
fn varlen_boundary() {
    let deltas: [u32; 12] = [
        0, 1, 0x7F, 0x80, 0x1FFF, 0x2000, 0x3FFF, 0x4000, 0x1F_FFFF, 0x20_0000, 0x3FF_FFFF,
        0xFFF_FFFF,
    ];
    let mut smf = Smf::new(Header::new(Format::SingleTrack, Timing::Metrical(u15::new(480))));
    let mut track = Vec::new();
    for &d in &deltas {
        track.push(note_on(d, 0, 60, 100));
    }
    track.push(ev(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)));
    smf.tracks.push(track);
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    let back = Smf::parse(&bytes).unwrap();
    let got: Vec<u32> = back.tracks[0]
        .iter()
        .map(|e| e.delta.as_int())
        .collect();
    let ok = got[..deltas.len()] == deltas[..];
    println!(
        "varlen len={} fnv={:016x} roundtrip={}",
        bytes.len(),
        fnv1a(&bytes),
        ok
    );
    for (i, g) in got.iter().enumerate() {
        println!("varlen[{i}] = {g}");
    }
    // 受限整数构造边界：Some/None 两分支
    println!(
        "u4 15={:?} 16={:?}",
        u4::try_from(15).map(|v| v.as_int()),
        u4::try_from(16).map(|v| v.as_int())
    );
    println!(
        "u7 127={:?} 128={:?}",
        u7::try_from(127).map(|v| v.as_int()),
        u7::try_from(128).map(|v| v.as_int())
    );
    println!(
        "u14 16383={:?} 16384={:?}",
        u14::try_from(16383).map(|v| v.as_int()),
        u14::try_from(16384).map(|v| v.as_int())
    );
    println!(
        "u15 32767={:?} 32768={:?}",
        u15::try_from(32767).map(|v| v.as_int()),
        u15::try_from(32768).map(|v| v.as_int())
    );
    println!(
        "u24 16777215={:?} 16777216={:?}",
        u24::try_from(16_777_215).map(|v| v.as_int()),
        u24::try_from(16_777_216).map(|v| v.as_int())
    );
    println!(
        "u28 268435455={:?} 268435456={:?}",
        u28::try_from(268_435_455).map(|v| v.as_int()),
        u28::try_from(268_435_456).map(|v| v.as_int())
    );
}

/// ④ RIFF/RMID 容器解包（riff::unwrap 路径）。
fn rmid_unwrap(smf_bytes: &[u8]) {
    let mut rmid = Vec::new();
    rmid.extend_from_slice(b"RIFF");
    rmid.extend_from_slice(&(4u32 + 8 + smf_bytes.len() as u32).to_le_bytes());
    rmid.extend_from_slice(b"RMID");
    rmid.extend_from_slice(b"data");
    rmid.extend_from_slice(&(smf_bytes.len() as u32).to_le_bytes());
    rmid.extend_from_slice(smf_bytes);
    let back = Smf::parse(&rmid).unwrap();
    println!(
        "rmid ok tracks={} fnv={:016x}",
        back.tracks.len(),
        fnv1a(&rmid)
    );
}

/// ⑤ 错误路径：打印 Display 文本（midly 错误串为 &'static str，确定）。
fn error_paths(smf_bytes: &[u8]) {
    for (label, input) in [
        ("empty", &b""[..]),
        ("badmagic", &b"XThd\x00\x00\x00\x06\x00\x01\x00\x01\x01\xE0"[..]),
        // RIFF 头但 formtype 不是 RMID
        ("riff-not-rmid", &b"RIFF\x04\x00\x00\x00WAVE"[..]),
        // 非法格式号 3
        ("fmt3", &b"MThd\x00\x00\x00\x06\x00\x03\x00\x01\x01\xE0"[..]),
    ] {
        match Smf::parse(input) {
            Ok(s) => println!("{label}: unexpected ok tracks={}", s.tracks.len()),
            Err(e) => println!("{label}: err={e}"),
        }
    }
    // 截断谱系：头后（lenient 下 0 轨解析成功）/轨中/末两字节
    for (label, cut) in [
        ("trunc-head", 14usize),
        ("trunc-mid", smf_bytes.len() / 2),
        ("trunc-tail", smf_bytes.len() - 2),
    ] {
        match Smf::parse(&smf_bytes[..cut]) {
            Ok(s) => println!("{label}: ok tracks={}", s.tracks.len()),
            Err(e) => println!("{label}: err={e}"),
        }
    }
}

/// ⑥ live 流事件 + 事件-字节映射。
fn live_and_bytemap(smf_bytes: &[u8]) {
    for (label, raw) in [
        ("noteon", &[0x93, 60, 100][..]),
        ("noteoff", &[0x83, 60, 64][..]),
        ("realtime", &[0xF8][..]),
        ("sysex", &[0xF0, 0x43, 0x7F][..]),
        ("empty", &[][..]),
    ] {
        match LiveEvent::parse(raw) {
            Ok(LiveEvent::Midi { channel, message }) => {
                let detail = match message {
                    MidiMessage::NoteOn { key, vel } => {
                        format!("noteon key={} vel={}", key.as_int(), vel.as_int())
                    }
                    MidiMessage::NoteOff { key, vel } => {
                        format!("noteoff key={} vel={}", key.as_int(), vel.as_int())
                    }
                    other => format!("other {:?}", other),
                };
                println!("live {label}: midi ch={} {detail}", channel.as_int());
            }
            Ok(LiveEvent::Realtime(sr)) => println!("live {label}: realtime {sr:?}"),
            Ok(LiveEvent::Common(c)) => {
                let tag = match &c {
                    midly::live::SystemCommon::SysEx(d) => format!("sysex len={}", d.len()),
                    midly::live::SystemCommon::MidiTimeCodeQuarterFrame(..) => "mtc".to_string(),
                    midly::live::SystemCommon::SongPosition(p) => {
                        format!("songpos {}", p.as_int())
                    }
                    midly::live::SystemCommon::SongSelect(s) => format!("songsel {}", s.as_int()),
                    midly::live::SystemCommon::TuneRequest => "tunereq".to_string(),
                    midly::live::SystemCommon::Undefined(b, _) => format!("undefined 0x{b:02x}"),
                };
                println!("live {label}: common {tag}");
            }
            Err(e) => println!("live {label}: err={e}"),
        }
    }
    let bm = SmfBytemap::parse(smf_bytes).unwrap();
    let lens: Vec<String> = bm.tracks[0]
        .iter()
        .map(|(raw, _)| raw.len().to_string())
        .collect();
    println!(
        "bytemap tracks={} t0-evlens={}",
        bm.tracks.len(),
        lens.join(",")
    );
}

fn main() {
    // ① 构建 → 写出 → 解析回放 → 再写出 roundtrip 不变量
    let smf = build_main_smf();
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    println!("smf len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));
    let parsed = Smf::parse(&bytes).unwrap();
    dump_smf("main", &parsed);
    let mut bytes2 = Vec::new();
    parsed.write_std(&mut bytes2).unwrap();
    println!(
        "rewrite len={} fnv={:016x} roundtrip={}",
        bytes2.len(),
        fnv1a(&bytes2),
        bytes == bytes2
    );
    let parsed2 = Smf::parse(&bytes2).unwrap();
    println!("reparse-eq={}", parsed == parsed2);

    format_matrix();
    varlen_boundary();
    rmid_unwrap(&bytes);
    error_paths(&bytes);
    live_and_bytemap(&bytes);
}
