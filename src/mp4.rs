//! Just enough MP4 to join the segments Media Foundation writes into one clip without
//! re-encoding: read each track's sample table, lay the chosen samples on one timeline and
//! write a fast-start file (index first, so Discord and browsers play it immediately).

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Decode time in the track's timescale.
    pub dts: u64,
    pub duration: u32,
    /// Composition offset (B-frames); zero for the encoders used here, kept for correctness.
    pub cts: i32,
    pub sync: bool,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub kind: Kind,
    pub timescale: u32,
    /// The sample entry (avc1 / mp4a box) describing the codec.
    pub entry: Vec<u8>,
    pub samples: Vec<Sample>,
    /// From the edit list: where sample time 0 lands on the movie timeline, in the track's
    /// timescale. Negative when the first part is decoded but not shown.
    pub start: i64,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn be16(b: &[u8], at: usize) -> io::Result<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or_else(|| invalid("truncated box"))
}

fn be32(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| invalid("truncated box"))
}

fn be64(b: &[u8], at: usize) -> io::Result<u64> {
    Ok((be32(b, at)? as u64) << 32 | be32(b, at + 4)? as u64)
}

/// A box as (type, payload, whole box).
type BoxRef<'a> = ([u8; 4], &'a [u8], &'a [u8]);

/// Child boxes of `b`.
fn boxes(b: &[u8]) -> io::Result<Vec<BoxRef<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= b.len() {
        let mut size = be32(b, at)? as usize;
        let kind = [b[at + 4], b[at + 5], b[at + 6], b[at + 7]];
        let mut header = 8;
        if size == 1 {
            size = be64(b, at + 8)? as usize;
            header = 16;
        } else if size == 0 {
            size = b.len() - at;
        }
        if size < header || at + size > b.len() {
            return Err(invalid("bad box size"));
        }
        out.push((kind, &b[at + header..at + size], &b[at..at + size]));
        at += size;
    }
    Ok(out)
}

fn child<'a>(b: &'a [u8], kind: &[u8; 4]) -> io::Result<&'a [u8]> {
    boxes(b)?
        .into_iter()
        .find(|(k, ..)| k == kind)
        .map(|(_, payload, _)| payload)
        .ok_or_else(|| invalid(&format!("missing {} box", String::from_utf8_lossy(kind))))
}

/// Finds the top-level `moov` box without loading the (large) `mdat`.
fn read_moov(f: &mut File) -> io::Result<Vec<u8>> {
    let len = f.metadata()?.len();
    let mut at = 0u64;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let mut header = 8;
        if size == 1 {
            f.read_exact(&mut h[8..])?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = len - at;
        }
        if size < header {
            return Err(invalid("bad top-level box"));
        }
        if &h[4..8] == b"moov" {
            let mut moov = vec![0; (size - header) as usize];
            f.read_exact(&mut moov)?;
            return Ok(moov);
        }
        at += size;
    }
    Err(invalid("no moov box (segment not finalized?)"))
}

pub fn read(path: &Path) -> io::Result<Vec<Track>> {
    let moov = read_moov(&mut File::open(path)?)?;
    let mvhd = child(&moov, b"mvhd")?;
    let movie_scale = if mvhd[0] == 1 {
        be32(mvhd, 20)?
    } else {
        be32(mvhd, 12)?
    } as i64;
    let mut tracks = Vec::new();
    for (kind, trak, _) in boxes(&moov)? {
        if &kind != b"trak" {
            continue;
        }
        let mdia = child(trak, b"mdia")?;
        let mdhd = child(mdia, b"mdhd")?;
        let timescale = if mdhd[0] == 1 {
            be32(mdhd, 20)?
        } else {
            be32(mdhd, 12)?
        };
        let handler = child(mdia, b"hdlr")?
            .get(8..12)
            .ok_or_else(|| invalid("short hdlr"))?;
        let kind = match handler {
            b"vide" => Kind::Video,
            b"soun" => Kind::Audio,
            _ => continue,
        };
        let stbl = child(child(mdia, b"minf")?, b"stbl")?;
        let stsd = child(stbl, b"stsd")?;
        let entry = boxes(stsd.get(8..).ok_or_else(|| invalid("short stsd"))?)?
            .first()
            .map(|(_, _, whole)| whole.to_vec())
            .ok_or_else(|| invalid("empty stsd"))?;
        let start = edit_start(trak, movie_scale, timescale as i64).unwrap_or(0);
        tracks.push(Track {
            kind,
            timescale,
            entry,
            samples: sample_table(stbl)?,
            start,
        });
    }
    Ok(tracks)
}

/// Empty edits delay the track; the first real edit's media time is skipped.
fn edit_start(trak: &[u8], movie_scale: i64, track_scale: i64) -> io::Result<i64> {
    let elst = child(child(trak, b"edts")?, b"elst")?;
    let wide = elst[0] == 1;
    let (mut delay, mut skip) = (0i64, 0i64);
    for i in 0..be32(elst, 4)? as usize {
        let (dur, media) = if wide {
            (
                be64(elst, 8 + i * 20)? as i64,
                be64(elst, 16 + i * 20)? as i64,
            )
        } else {
            (
                be32(elst, 8 + i * 12)? as i64,
                be32(elst, 12 + i * 12)? as i32 as i64,
            )
        };
        if media == -1 {
            delay += dur * track_scale / movie_scale;
        } else {
            skip = media;
            break;
        }
    }
    Ok(delay - skip)
}

fn sample_table(stbl: &[u8]) -> io::Result<Vec<Sample>> {
    let stsz = child(stbl, b"stsz")?;
    let (fixed, count) = (be32(stsz, 4)?, be32(stsz, 8)? as usize);
    let sizes: Vec<u32> = if fixed != 0 {
        vec![fixed; count]
    } else {
        (0..count)
            .map(|i| be32(stsz, 12 + i * 4))
            .collect::<io::Result<_>>()?
    };

    let chunks: Vec<u64> = if let Ok(stco) = child(stbl, b"stco") {
        (0..be32(stco, 4)? as usize)
            .map(|i| be32(stco, 8 + i * 4).map(u64::from))
            .collect::<io::Result<_>>()?
    } else {
        let co64 = child(stbl, b"co64")?;
        (0..be32(co64, 4)? as usize)
            .map(|i| be64(co64, 8 + i * 8))
            .collect::<io::Result<_>>()?
    };

    let stsc = child(stbl, b"stsc")?;
    let runs: Vec<(usize, usize)> = (0..be32(stsc, 4)? as usize)
        .map(|i| {
            Ok((
                be32(stsc, 8 + i * 12)? as usize,
                be32(stsc, 12 + i * 12)? as usize,
            ))
        })
        .collect::<io::Result<_>>()?;
    let mut offsets = Vec::with_capacity(count);
    for (c, &chunk_offset) in chunks.iter().enumerate() {
        let per_chunk = runs
            .iter()
            .rev()
            .find(|(first, _)| *first <= c + 1)
            .map_or(0, |r| r.1);
        let mut at = chunk_offset;
        for _ in 0..per_chunk {
            let Some(&size) = sizes.get(offsets.len()) else {
                break;
            };
            offsets.push(at);
            at += size as u64;
        }
    }
    if offsets.len() != count {
        return Err(invalid("chunk table does not cover every sample"));
    }

    let stts = child(stbl, b"stts")?;
    let mut durations = Vec::with_capacity(count);
    for i in 0..be32(stts, 4)? as usize {
        let (n, delta) = (be32(stts, 8 + i * 8)?, be32(stts, 12 + i * 8)?);
        durations.extend(std::iter::repeat_n(delta, n as usize));
    }
    durations.resize(count, durations.last().copied().unwrap_or(0));

    let mut cts = vec![0i32; count];
    if let Ok(ctts) = child(stbl, b"ctts") {
        let mut i = 0;
        for e in 0..be32(ctts, 4)? as usize {
            let (n, off) = (be32(ctts, 8 + e * 8)?, be32(ctts, 12 + e * 8)? as i32);
            for _ in 0..n {
                if let Some(c) = cts.get_mut(i) {
                    *c = off;
                }
                i += 1;
            }
        }
    }

    let mut sync = vec![true; count];
    if let Ok(stss) = child(stbl, b"stss") {
        sync.fill(false);
        for i in 0..be32(stss, 4)? as usize {
            if let Some(s) = sync.get_mut((be32(stss, 8 + i * 4)? as usize).wrapping_sub(1)) {
                *s = true;
            }
        }
    }

    let mut dts = 0u64;
    Ok((0..count)
        .map(|i| {
            let s = Sample {
                offset: offsets[i],
                size: sizes[i],
                dts,
                duration: durations[i],
                cts: cts[i],
                sync: sync[i],
            };
            dts += durations[i] as u64;
            s
        })
        .collect())
}

/// One segment file with its start on the shared clock (100 ns units).
pub struct Source {
    pub path: PathBuf,
    pub start: i64,
}

const TICKS: i64 = 10_000_000;

struct Placed {
    src: usize,
    offset: u64,
    size: u32,
    /// Decode time on the clip timeline, in the track's timescale.
    dts: u64,
    cts: i32,
    sync: bool,
    entry: u32,
}

struct OutTrack {
    kind: Kind,
    timescale: u32,
    entries: Vec<Vec<u8>>,
    samples: Vec<Placed>,
    last_duration: u32,
    /// Where the first sample lands on the clip timeline, in the track's timescale. Negative
    /// means the first part is decoded but not shown (video starting on an earlier keyframe).
    start: i64,
}

/// Writes the part of `sources` from `from` (shared clock) onward as one MP4.
pub fn join(sources: &[Source], from: i64, out: &Path) -> io::Result<()> {
    let parsed: Vec<Vec<Track>> = sources
        .iter()
        .map(|s| read(&s.path))
        .collect::<io::Result<_>>()?;
    // Shared-clock time of a sample, honoring the source's own edit list.
    let abs = |src: usize, t: &Track, dts: u64| {
        sources[src].start
            + ((dts as i64 + t.start) as i128 * TICKS as i128 / t.timescale as i128) as i64
    };

    // The picture starts on the latest video keyframe not after `from` (or the first one overall).
    let video = |i: usize| parsed[i].iter().filter(|t| t.kind == Kind::Video);
    let mut key = None;
    for i in 0..parsed.len() {
        for t in video(i) {
            for s in t.samples.iter().filter(|s| s.sync) {
                let at = abs(i, t, s.dts);
                if at <= from || key.is_none() {
                    key = Some(at);
                }
            }
        }
    }
    let key = key.ok_or_else(|| invalid("no video keyframe to start the clip on"))?;
    // Decoding must begin on that keyframe, which can be far back on a static screen (keyframes
    // come every N frames, and a still screen sends few). An edit list hides everything before
    // `from`, so the clip is exactly as long as asked. Before the first keyframe there is no
    // picture, so a clip longer than the recording starts there.
    let origin = from.max(key);

    let mut outs: Vec<OutTrack> = Vec::new();
    for kind in [Kind::Video, Kind::Audio] {
        let mut o: Option<OutTrack> = None;
        for (i, tracks) in parsed.iter().enumerate() {
            for t in tracks.iter().filter(|t| t.kind == kind) {
                let ot = o.get_or_insert_with(|| OutTrack {
                    kind,
                    timescale: t.timescale,
                    entries: Vec::new(),
                    samples: Vec::new(),
                    last_duration: 0,
                    start: 0,
                });
                let entry = match ot.entries.iter().position(|e| *e == t.entry) {
                    Some(p) => p,
                    None => {
                        ot.entries.push(t.entry.clone());
                        ot.entries.len() - 1
                    }
                } as u32
                    + 1;
                for s in &t.samples {
                    let at = abs(i, t, s.dts);
                    if at < if kind == Kind::Video { key } else { origin } {
                        continue;
                    }
                    let scale =
                        |ticks: i64| (ticks as i128 * ot.timescale as i128 / TICKS as i128) as i64;
                    if ot.samples.is_empty() {
                        ot.start = scale(at - origin);
                    }
                    let mut dts = (scale(at - origin) - ot.start) as u64;
                    // Rounding across segments must never move time backwards.
                    if let Some(prev) = ot.samples.last() {
                        dts = dts.max(prev.dts + 1);
                    }
                    ot.samples.push(Placed {
                        src: i,
                        offset: s.offset,
                        size: s.size,
                        dts,
                        cts: s.cts,
                        sync: s.sync,
                        entry,
                    });
                    ot.last_duration = s.duration;
                }
            }
        }
        if let Some(o) = o.filter(|o| !o.samples.is_empty()) {
            outs.push(o);
        }
    }
    write(sources, &outs, out)
}

fn put_box(out: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    body(out);
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

fn u32s(out: &mut Vec<u8>, values: &[u32]) {
    values
        .iter()
        .for_each(|v| out.extend_from_slice(&v.to_be_bytes()));
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];
const MOVIE_TIMESCALE: u32 = 1000;

fn durations(t: &OutTrack) -> Vec<u32> {
    let mut d: Vec<u32> = t
        .samples
        .windows(2)
        .map(|w| (w[1].dts - w[0].dts) as u32)
        .collect();
    d.push(t.last_duration.max(1));
    d
}

fn track_length(t: &OutTrack) -> u64 {
    t.samples.last().map_or(0, |s| s.dts) + t.last_duration as u64
}

/// The part of the media shown: an empty edit delays a late track, a media offset skips the
/// decoded-but-hidden lead-in. MP4 sample tables always start at zero, so without the edit list
/// a track would play early.
fn shown(t: &OutTrack) -> (u64, u64, u64) {
    let delay = t.start.max(0) as u64;
    let skip = (-t.start).max(0) as u64;
    (delay, skip, track_length(t).saturating_sub(skip))
}

fn moov(tracks: &[OutTrack], chunk_offsets: &[Vec<u64>], wide: bool) -> Vec<u8> {
    let movie_len = |t: &OutTrack| {
        let (delay, _, visible) = shown(t);
        (delay + visible) * MOVIE_TIMESCALE as u64 / t.timescale as u64
    };
    let longest = tracks.iter().map(movie_len).max().unwrap_or(0) as u32;
    let mut b = Vec::new();
    put_box(&mut b, b"moov", |b| {
        put_box(b, b"mvhd", |b| {
            u32s(b, &[0, 0, 0, MOVIE_TIMESCALE, longest, 0x10000]);
            b.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // volume 1.0 + reserved
            u32s(b, &MATRIX);
            u32s(b, &[0; 6]);
            u32s(b, &[tracks.len() as u32 + 1]);
        });
        for (id, t) in tracks.iter().enumerate() {
            let entry = &t.entries[0];
            let (w, h) = match t.kind {
                Kind::Video => (
                    be16(entry, 32).unwrap_or(0) as u32,
                    be16(entry, 34).unwrap_or(0) as u32,
                ),
                Kind::Audio => (0, 0),
            };
            put_box(b, b"trak", |b| {
                put_box(b, b"tkhd", |b| {
                    u32s(b, &[3, 0, 0, id as u32 + 1, 0, movie_len(t) as u32, 0, 0]);
                    u32s(
                        b,
                        &[
                            0,
                            if t.kind == Kind::Audio {
                                0x0100_0000
                            } else {
                                0
                            },
                        ],
                    );
                    u32s(b, &MATRIX);
                    u32s(b, &[w << 16, h << 16]);
                });
                let (delay, skip, visible) = shown(t);
                if delay > 0 || skip > 0 {
                    let to_movie =
                        |v: u64| (v * MOVIE_TIMESCALE as u64 / t.timescale as u64) as u32;
                    put_box(b, b"edts", |b| {
                        put_box(b, b"elst", |b| {
                            let entries = if delay > 0 { 2 } else { 1 };
                            u32s(b, &[0, entries]);
                            if delay > 0 {
                                u32s(b, &[to_movie(delay), u32::MAX, 0x10000]); // empty edit
                            }
                            u32s(b, &[to_movie(visible), skip as u32, 0x10000]);
                        });
                    });
                }
                put_box(b, b"mdia", |b| {
                    put_box(b, b"mdhd", |b| {
                        u32s(b, &[0, 0, 0, t.timescale, track_length(t) as u32]);
                        b.extend_from_slice(&[0x55, 0xC4, 0, 0]); // language "und"
                    });
                    put_box(b, b"hdlr", |b| {
                        u32s(b, &[0, 0]);
                        b.extend_from_slice(if t.kind == Kind::Video {
                            b"vide"
                        } else {
                            b"soun"
                        });
                        u32s(b, &[0, 0, 0]);
                        b.extend_from_slice(if t.kind == Kind::Video {
                            b"Video\0"
                        } else {
                            b"Audio\0"
                        });
                    });
                    put_box(b, b"minf", |b| {
                        match t.kind {
                            Kind::Video => put_box(b, b"vmhd", |b| u32s(b, &[1, 0, 0])),
                            Kind::Audio => put_box(b, b"smhd", |b| u32s(b, &[0, 0])),
                        }
                        put_box(b, b"dinf", |b| {
                            put_box(b, b"dref", |b| {
                                u32s(b, &[0, 1]);
                                put_box(b, b"url ", |b| u32s(b, &[1]));
                            });
                        });
                        put_box(b, b"stbl", |b| stbl(b, t, &chunk_offsets[id], wide));
                    });
                });
            });
        }
    });
    b
}

fn stbl(b: &mut Vec<u8>, t: &OutTrack, offsets: &[u64], wide: bool) {
    put_box(b, b"stsd", |b| {
        u32s(b, &[0, t.entries.len() as u32]);
        t.entries.iter().for_each(|e| b.extend_from_slice(e));
    });
    let runs = |values: Vec<u32>| {
        let mut r: Vec<(u32, u32)> = Vec::new();
        for v in values {
            match r.last_mut() {
                Some((n, last)) if *last == v => *n += 1,
                _ => r.push((1, v)),
            }
        }
        r
    };
    let stts = runs(durations(t));
    put_box(b, b"stts", |b| {
        u32s(b, &[0, stts.len() as u32]);
        stts.iter().for_each(|&(n, d)| u32s(b, &[n, d]));
    });
    if t.samples.iter().any(|s| s.cts != 0) {
        let ctts = runs(t.samples.iter().map(|s| s.cts as u32).collect());
        put_box(b, b"ctts", |b| {
            u32s(b, &[1 << 24, ctts.len() as u32]);
            ctts.iter().for_each(|&(n, c)| u32s(b, &[n, c]));
        });
    }
    if t.samples.iter().any(|s| !s.sync) {
        let keys: Vec<u32> = t
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.sync)
            .map(|(i, _)| i as u32 + 1)
            .collect();
        put_box(b, b"stss", |b| {
            u32s(b, &[0, keys.len() as u32]);
            u32s(b, &keys);
        });
    }
    // One sample per chunk; a new stsc run only where the sample entry changes.
    let mut stsc: Vec<[u32; 3]> = Vec::new();
    for (i, s) in t.samples.iter().enumerate() {
        if stsc.last().is_none_or(|r| r[2] != s.entry) {
            stsc.push([i as u32 + 1, 1, s.entry]);
        }
    }
    put_box(b, b"stsc", |b| {
        u32s(b, &[0, stsc.len() as u32]);
        stsc.iter().for_each(|r| u32s(b, r));
    });
    put_box(b, b"stsz", |b| {
        u32s(b, &[0, 0, t.samples.len() as u32]);
        t.samples.iter().for_each(|s| u32s(b, &[s.size]));
    });
    if wide {
        put_box(b, b"co64", |b| {
            u32s(b, &[0, offsets.len() as u32]);
            offsets
                .iter()
                .for_each(|o| b.extend_from_slice(&o.to_be_bytes()));
        });
    } else {
        put_box(b, b"stco", |b| {
            u32s(b, &[0, offsets.len() as u32]);
            offsets.iter().for_each(|&o| u32s(b, &[o as u32]));
        });
    }
}

const FTYP: &[u8] = &[
    0, 0, 0, 28, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0, 0, 2, 0, b'i', b's', b'o',
    b'm', b'i', b's', b'o', b'2', b'a', b'v', b'c', b'1',
];

fn write(sources: &[Source], tracks: &[OutTrack], out: &Path) -> io::Result<()> {
    // Interleave tracks by time so players read the file front to back.
    let mut order: Vec<(usize, usize)> = tracks
        .iter()
        .enumerate()
        .flat_map(|(t, tr)| (0..tr.samples.len()).map(move |i| (t, i)))
        .collect();
    order.sort_by_key(|&(t, i)| {
        tracks[t].samples[i].dts as u128 * 1_000_000 / tracks[t].timescale as u128
    });

    let payload: u64 = tracks
        .iter()
        .flat_map(|t| &t.samples)
        .map(|s| s.size as u64)
        .sum();
    let wide = payload > u32::MAX as u64 / 2;
    let header = if payload + 8 > u32::MAX as u64 { 16 } else { 8 };
    let place = |base: u64| {
        let mut offsets: Vec<Vec<u64>> = tracks.iter().map(|t| vec![0; t.samples.len()]).collect();
        let mut at = base;
        for &(t, i) in &order {
            offsets[t][i] = at;
            at += tracks[t].samples[i].size as u64;
        }
        offsets
    };
    // The moov size does not depend on the offset values, so lay it out once to learn its size.
    let moov_len = moov(tracks, &place(0), wide).len() as u64;
    let base = FTYP.len() as u64 + moov_len + header;
    let moov = moov(tracks, &place(base), wide);

    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    w.write_all(FTYP)?;
    w.write_all(&moov)?;
    if header == 16 {
        w.write_all(&1u32.to_be_bytes())?;
        w.write_all(b"mdat")?;
        w.write_all(&(payload + 16).to_be_bytes())?;
    } else {
        w.write_all(&(payload as u32 + 8).to_be_bytes())?;
        w.write_all(b"mdat")?;
    }
    let mut files: Vec<File> = sources
        .iter()
        .map(|s| File::open(&s.path))
        .collect::<io::Result<_>>()?;
    let mut buf = Vec::new();
    for &(t, i) in &order {
        let s = &tracks[t].samples[i];
        buf.resize(s.size as usize, 0);
        let f = &mut files[s.src];
        f.seek(SeekFrom::Start(s.offset))?;
        f.read_exact(&mut buf)?;
        w.write_all(&buf)?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO_TS: u32 = 60_000;
    const AUDIO_TS: u32 = 48_000;

    /// A fake codec entry with the width/height fields where avc1 keeps them.
    fn entry(kind: &[u8; 4], w: u16, h: u16) -> Vec<u8> {
        let mut e = vec![0u8; 86];
        e[..4].copy_from_slice(&86u32.to_be_bytes());
        e[4..8].copy_from_slice(kind);
        e[32..34].copy_from_slice(&w.to_be_bytes());
        e[34..36].copy_from_slice(&h.to_be_bytes());
        e
    }

    /// Payload bytes that identify (track, sample index), so a join can be checked byte by byte.
    fn payload(track: u8, i: usize, size: u32) -> Vec<u8> {
        let mut p = vec![track; size as usize];
        p[1..5].copy_from_slice(&(i as u32).to_be_bytes());
        p
    }

    /// Writes a segment like Media Foundation does: `frames` video frames at 60 fps with a
    /// keyframe every second, plus `audio` AAC-sized packets.
    fn segment(dir: &Path, name: &str, frames: usize, audio: usize, tag: u8) -> PathBuf {
        let raw = dir.join(format!("{name}.raw"));
        let mut data = Vec::new();
        let mut video = Vec::new();
        for i in 0..frames {
            let size = 200 + (i % 7) as u32;
            video.push(Placed {
                src: 0,
                offset: data.len() as u64,
                size,
                dts: i as u64 * 1000,
                cts: 0,
                sync: i % 60 == 0,
                entry: 1,
            });
            data.extend(payload(tag, i, size));
        }
        let mut sound = Vec::new();
        for i in 0..audio {
            sound.push(Placed {
                src: 0,
                offset: data.len() as u64,
                size: 50,
                dts: i as u64 * 1024,
                cts: 0,
                sync: true,
                entry: 1,
            });
            data.extend(payload(tag + 100, i, 50));
        }
        std::fs::write(&raw, data).unwrap();
        let tracks = [
            OutTrack {
                kind: Kind::Video,
                timescale: VIDEO_TS,
                entries: vec![entry(b"avc1", 1920, 1080)],
                samples: video,
                last_duration: 1000,
                start: 0,
            },
            OutTrack {
                kind: Kind::Audio,
                timescale: AUDIO_TS,
                entries: vec![entry(b"mp4a", 0, 0)],
                samples: sound,
                last_duration: 1024,
                start: 0,
            },
        ];
        let out = dir.join(format!("{name}.mp4"));
        write(
            &[Source {
                path: raw,
                start: 0,
            }],
            &tracks,
            &out,
        )
        .unwrap();
        out
    }

    fn bytes_of(path: &Path, s: &Sample) -> Vec<u8> {
        let mut f = File::open(path).unwrap();
        f.seek(SeekFrom::Start(s.offset)).unwrap();
        let mut b = vec![0; s.size as usize];
        f.read_exact(&mut b).unwrap();
        b
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipper-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn written_segment_reads_back_exactly() {
        let dir = temp_dir("read");
        let seg = segment(&dir, "a", 600, 469, 1);
        let tracks = read(&seg).unwrap();
        let v = tracks.iter().find(|t| t.kind == Kind::Video).unwrap();
        let a = tracks.iter().find(|t| t.kind == Kind::Audio).unwrap();
        assert_eq!(
            (v.timescale, v.samples.len(), a.samples.len()),
            (VIDEO_TS, 600, 469)
        );
        assert_eq!(v.samples.iter().filter(|s| s.sync).count(), 10);
        assert!(v.samples.iter().all(|s| s.duration == 1000));
        assert_eq!(
            bytes_of(&seg, &v.samples[123]),
            payload(1, 123, 200 + (123 % 7) as u32)
        );
        assert_eq!(bytes_of(&seg, &a.samples[400]), payload(101, 400, 50));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn join_starts_on_a_keyframe_and_keeps_every_byte() {
        let dir = temp_dir("join");
        let a = segment(&dir, "a", 600, 469, 1);
        let b = segment(&dir, "b", 600, 469, 2);
        let out = dir.join("clip.mp4");
        let sources = [
            Source {
                path: a.clone(),
                start: 0,
            },
            Source {
                path: b.clone(),
                start: 10 * TICKS,
            },
        ];
        // Ask for everything after 12.5 s: the clip must open on the 12 s keyframe.
        join(&sources, 12 * TICKS + TICKS / 2, &out).unwrap();

        let tracks = read(&out).unwrap();
        let v = tracks.iter().find(|t| t.kind == Kind::Video).unwrap();
        let au = tracks.iter().find(|t| t.kind == Kind::Audio).unwrap();
        assert!(v.samples[0].sync, "decoding must start on a keyframe");
        assert_eq!(v.samples.len(), 480, "8 s of video from the 12 s keyframe");
        assert_eq!(
            v.start,
            -(VIDEO_TS as i64) / 2,
            "the half second before 12.5 s is hidden"
        );
        assert!(v.samples.windows(2).all(|w| w[1].dts > w[0].dts));
        assert!(au.samples.windows(2).all(|w| w[1].dts > w[0].dts));
        // First video sample is frame 120 of segment b; the bytes travel unchanged.
        assert_eq!(
            bytes_of(&out, &v.samples[0]),
            payload(2, 120, 200 + (120 % 7) as u32)
        );
        assert_eq!(
            bytes_of(&out, v.samples.last().unwrap()),
            payload(2, 599, 200 + (599 % 7) as u32)
        );
        // Audio starts at the same moment as the video.
        let first_audio = bytes_of(&out, &au.samples[0]);
        let index = u32::from_be_bytes(first_audio[1..5].try_into().unwrap()) as u64;
        assert_eq!(first_audio[0], 102);
        assert_eq!(
            index,
            (5 * AUDIO_TS as u64 / 2).div_ceil(1024),
            "first audio packet at or after 12.5 s"
        );
        let audio_at = au.start as f64 / AUDIO_TS as f64;
        assert!(
            (0.0..0.03).contains(&audio_at),
            "audio starts within one packet of the clip start: {audio_at}"
        );
        let shown = |t: &Track| {
            (t.start + t.samples.last().map(|s| s.dts + s.duration as u64).unwrap() as i64) as f64
                / t.timescale as f64
        };
        assert!(
            (shown(v) - 7.5).abs() < 0.02 && (shown(au) - 7.5).abs() < 0.03,
            "{} {}",
            shown(v),
            shown(au)
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn join_spans_segments_and_covers_gaps() {
        let dir = temp_dir("span");
        // A static screen: one frame, then 10 s of audio only.
        let a = segment(&dir, "a", 1, 469, 1);
        let b = segment(&dir, "b", 600, 469, 2);
        let out = dir.join("clip.mp4");
        let sources = [
            Source { path: a, start: 0 },
            Source {
                path: b,
                start: 10 * TICKS,
            },
        ];
        join(&sources, 0, &out).unwrap();
        let tracks = read(&out).unwrap();
        let v = tracks.iter().find(|t| t.kind == Kind::Video).unwrap();
        let au = tracks.iter().find(|t| t.kind == Kind::Audio).unwrap();
        assert_eq!(v.samples.len(), 601);
        // The single static frame is held until segment b begins at 10 s.
        assert_eq!(v.samples[0].duration, 10 * VIDEO_TS);
        assert_eq!(au.samples.len(), 938);
        let end = |t: &Track| {
            t.samples.last().map(|s| s.dts + s.duration as u64).unwrap() as f64 / t.timescale as f64
        };
        assert!(
            (end(v) - 20.0).abs() < 0.05 && (end(au) - 20.0).abs() < 0.1,
            "{} {}",
            end(v),
            end(au)
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_files_without_an_index() {
        let dir = temp_dir("bad");
        let p = dir.join("bad.mp4");
        std::fs::write(&p, b"\0\0\0\x10mdat12345678").unwrap();
        assert!(read(&p).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn static_screen_clip_starts_at_the_requested_time() {
        let dir = temp_dir("idle");
        // One frame at 0 s, then nothing on screen until segment b at 10 s.
        let a = segment(&dir, "a", 1, 469, 1);
        let b = segment(&dir, "b", 600, 469, 2);
        let out = dir.join("clip.mp4");
        let sources = [
            Source { path: a, start: 0 },
            Source {
                path: b,
                start: 10 * TICKS,
            },
        ];
        join(&sources, 6 * TICKS, &out).unwrap();
        let tracks = read(&out).unwrap();
        let v = tracks.iter().find(|t| t.kind == Kind::Video).unwrap();
        // The idle frame is decoded from 0 s but shown only from 6 s: a 14 s clip, not 20 s.
        assert!(v.samples[0].sync);
        assert_eq!(v.samples[0].duration, 10 * VIDEO_TS);
        assert_eq!(v.start, -6 * VIDEO_TS as i64);
        let end = (v.start + v.samples.last().map(|s| s.dts + s.duration as u64).unwrap() as i64)
            as f64
            / VIDEO_TS as f64;
        assert!((end - 14.0).abs() < 0.05, "{end}");
        std::fs::remove_dir_all(dir).ok();
    }
}
