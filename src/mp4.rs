//! Minimal MP4 (ISO BMFF) demuxer.
//!
//! Only what HLS packaging needs: the movie header, one entry per track and the
//! flattened sample table (offset, size, timing, sync flag). Sample *data* is
//! never read here, only located, so parsing a multi-gigabyte file costs the
//! size of its `moov` box.

use std::io;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Boxes we walk into instead of skipping over.
const CONTAINERS: [&[u8; 4]; 6] = [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts"];

/// A `moov` bigger than this is not something we are willing to buffer.
const MAX_MOOV: usize = 64 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Clone, Copy)]
pub struct Sample {
    /// Absolute byte offset in the source file.
    pub offset: u64,
    pub size: u32,
    /// Decode timestamp, in the track timescale.
    pub dts: u64,
    /// Decode duration, in the track timescale.
    pub duration: u32,
    /// Composition offset (`ctts`), in the track timescale.
    pub cts_offset: i32,
    pub is_sync: bool,
}

pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    pub timescale: u32,
    /// Sum of the sample durations, in the track timescale.
    pub duration: u64,
    /// 16.16 fixed point values straight out of `tkhd`.
    pub width: u32,
    pub height: u32,
    pub language: u16,
    /// Raw payload of the `stsd` box, replayed verbatim into the init segment.
    pub stsd: Vec<u8>,
    pub samples: Vec<Sample>,
}

impl Track {
    /// RFC 6381 codec identifier, when we can read it out of the sample entry.
    pub fn codec(&self) -> Option<String> {
        let (format, body) = stsd_first_entry(&self.stsd)?;
        match &format {
            b"avc1" | b"avc3" => {
                // 78 bytes of VisualSampleEntry precede the extension boxes.
                let avcc = find_box(body.get(78..)?, b"avcC")?;
                let p = avcc.get(1..4)?;
                Some(format!(
                    "{}.{:02x}{:02x}{:02x}",
                    fourcc(&format),
                    p[0],
                    p[1],
                    p[2]
                ))
            }
            b"hvc1" | b"hev1" => {
                let hvcc = find_box(body.get(78..)?, b"hvcC")?;
                // general_profile_idc sits in the low 5 bits of byte 1.
                let profile = hvcc.get(1)? & 0x1f;
                let level = *hvcc.get(12)?;
                Some(format!("{}.{}.4.L{}.B0", fourcc(&format), profile, level))
            }
            b"mp4a" => {
                // 28 bytes of AudioSampleEntry precede the extension boxes.
                let esds = body.get(28..).and_then(|b| find_box(b, b"esds"));
                let aot = esds.and_then(audio_object_type).unwrap_or(2);
                Some(format!("mp4a.40.{aot}"))
            }
            b"Opus" => Some(String::from("opus")),
            b"ac-3" => Some(String::from("ac-3")),
            b"ec-3" => Some(String::from("ec-3")),
            _ => Some(fourcc(&format)),
        }
    }

    /// Index of the first sample whose decode time is at or after `dts`.
    pub fn sample_at(&self, dts: u64) -> usize {
        self.samples.partition_point(|s| s.dts < dts)
    }
}

pub struct Mp4 {
    pub timescale: u32,
    pub tracks: Vec<Track>,
}

impl Mp4 {
    /// Longest track, in seconds.
    pub fn duration(&self) -> f64 {
        self.tracks
            .iter()
            .map(|t| t.duration as f64 / t.timescale as f64)
            .fold(0.0, f64::max)
    }

    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }
}

/// Reads the `moov` box of `file` and flattens every track's sample table.
pub async fn parse(file: &mut File) -> io::Result<Mp4> {
    let len = file.seek(io::SeekFrom::End(0)).await?;
    let mut pos = 0u64;
    let mut moov = None;

    // Walk the top level only: `moov` may sit before or after a huge `mdat`.
    while pos + 8 <= len {
        let (size, kind, header) = read_header(file, pos).await?;
        let size = if size == 0 { len - pos } else { size };
        if size < header || pos + size > len {
            break;
        }
        if &kind == b"moov" {
            let body_len = usize::try_from(size - header)
                .map_err(|_| invalid("moov box is too large to parse"))?;
            if body_len > MAX_MOOV {
                return Err(invalid("moov box is too large to parse"));
            }
            let mut buf = vec![0u8; body_len];
            file.seek(io::SeekFrom::Start(pos + header)).await?;
            file.read_exact(&mut buf).await?;
            moov = Some(buf);
            break;
        }
        pos += size;
    }

    let moov = moov.ok_or_else(|| invalid("no moov box found"))?;
    parse_moov(&moov)
}

fn parse_moov(moov: &[u8]) -> io::Result<Mp4> {
    let mvhd = find_box(moov, b"mvhd").ok_or_else(|| invalid("no mvhd box found"))?;
    let timescale = match version(mvhd) {
        0 => u32be(mvhd, 12)?,
        _ => u32be(mvhd, 20)?,
    };

    let mut tracks = Vec::new();
    for trak in boxes(moov, b"trak") {
        if let Some(track) = parse_trak(trak)? {
            tracks.push(track);
        }
    }
    if tracks.is_empty() {
        return Err(invalid("no playable track found"));
    }

    Ok(Mp4 {
        timescale: timescale.max(1),
        tracks,
    })
}

fn parse_trak(trak: &[u8]) -> io::Result<Option<Track>> {
    let tkhd = find_box(trak, b"tkhd").ok_or_else(|| invalid("no tkhd box found"))?;
    let (id, width, height) = match version(tkhd) {
        0 => (u32be(tkhd, 12)?, u32be(tkhd, 76)?, u32be(tkhd, 80)?),
        _ => (u32be(tkhd, 20)?, u32be(tkhd, 88)?, u32be(tkhd, 92)?),
    };

    let mdia = find_box(trak, b"mdia").ok_or_else(|| invalid("no mdia box found"))?;
    let mdhd = find_box(mdia, b"mdhd").ok_or_else(|| invalid("no mdhd box found"))?;
    let (timescale, language) = match version(mdhd) {
        0 => (u32be(mdhd, 12)?, u16be(mdhd, 20)?),
        _ => (u32be(mdhd, 20)?, u16be(mdhd, 32)?),
    };

    let hdlr = find_box(mdia, b"hdlr").ok_or_else(|| invalid("no hdlr box found"))?;
    let kind = match hdlr.get(8..12) {
        Some(b"vide") => TrackKind::Video,
        Some(b"soun") => TrackKind::Audio,
        _ => TrackKind::Other,
    };
    // Subtitle, timed-metadata and chapter tracks are dropped: the packager has
    // no way to carry them and a player would reject the resulting segment.
    if kind == TrackKind::Other {
        return Ok(None);
    }

    let stbl = find_box(mdia, b"stbl").ok_or_else(|| invalid("no stbl box found"))?;
    let stsd = find_box(stbl, b"stsd")
        .ok_or_else(|| invalid("no stsd box found"))?
        .to_vec();

    let samples = build_samples(stbl)?;
    if samples.is_empty() {
        return Ok(None);
    }
    let duration = samples.last().map_or(0, |s| s.dts + s.duration as u64);

    Ok(Some(Track {
        id,
        kind,
        timescale: timescale.max(1),
        duration,
        width,
        height,
        language,
        stsd,
        samples,
    }))
}

/// Joins `stts`/`ctts`/`stsc`/`stsz`/`stco`/`stss` into one flat sample list.
fn build_samples(stbl: &[u8]) -> io::Result<Vec<Sample>> {
    let sizes = parse_stsz(stbl)?;
    let count = sizes.len();
    if count == 0 {
        return Ok(Vec::new());
    }

    let chunk_offsets = parse_chunk_offsets(stbl)?;
    let stsc = find_box(stbl, b"stsc").ok_or_else(|| invalid("no stsc box found"))?;
    let stsc_count = usize::try_from(u32be(stsc, 4)?).unwrap_or(0);

    let mut samples = Vec::with_capacity(count);
    let mut sample = 0usize;

    // `stsc` is run-length encoded over chunks: each entry holds until the
    // `first_chunk` of the next one.
    for entry in 0..stsc_count {
        let base = 8 + entry * 12;
        let first_chunk = u32be(stsc, base)? as usize;
        let per_chunk = u32be(stsc, base + 4)? as usize;
        let next_first = if entry + 1 < stsc_count {
            u32be(stsc, base + 12)? as usize
        } else {
            chunk_offsets.len() + 1
        };
        if first_chunk == 0 || next_first <= first_chunk {
            return Err(invalid("malformed stsc box"));
        }

        for chunk in first_chunk..next_first {
            let Some(&chunk_offset) = chunk_offsets.get(chunk - 1) else {
                break;
            };
            let mut offset = chunk_offset;
            for _ in 0..per_chunk {
                if sample >= count {
                    break;
                }
                let size = sizes[sample];
                samples.push(Sample {
                    offset,
                    size,
                    dts: 0,
                    duration: 0,
                    cts_offset: 0,
                    is_sync: false,
                });
                offset = offset.saturating_add(u64::from(size));
                sample += 1;
            }
        }
        if sample >= count {
            break;
        }
    }

    if samples.len() != count {
        return Err(invalid("sample table is inconsistent"));
    }

    apply_timing(stbl, &mut samples)?;
    apply_sync(stbl, &mut samples)?;
    Ok(samples)
}

fn apply_timing(stbl: &[u8], samples: &mut [Sample]) -> io::Result<()> {
    let stts = find_box(stbl, b"stts").ok_or_else(|| invalid("no stts box found"))?;
    let entries = usize::try_from(u32be(stts, 4)?).unwrap_or(0);
    let mut index = 0usize;
    let mut dts = 0u64;
    for entry in 0..entries {
        let base = 8 + entry * 8;
        let run = u32be(stts, base)?;
        let delta = u32be(stts, base + 4)?;
        for _ in 0..run {
            let Some(sample) = samples.get_mut(index) else {
                break;
            };
            sample.dts = dts;
            sample.duration = delta;
            dts += u64::from(delta);
            index += 1;
        }
    }
    // A short `stts` would otherwise leave trailing samples at zero duration and
    // collapse the timeline; repeat the last known delta instead.
    if index < samples.len() {
        let delta = index
            .checked_sub(1)
            .and_then(|i| samples.get(i))
            .map_or(0, |s| s.duration);
        for sample in &mut samples[index..] {
            sample.dts = dts;
            sample.duration = delta;
            dts += u64::from(delta);
        }
    }

    if let Some(ctts) = find_box(stbl, b"ctts") {
        let entries = usize::try_from(u32be(ctts, 4)?).unwrap_or(0);
        let mut index = 0usize;
        for entry in 0..entries {
            let base = 8 + entry * 8;
            let run = u32be(ctts, base)?;
            // Version 0 declares the field unsigned, but encoders in the wild
            // still write negative offsets there, so read both as i32.
            let offset = u32be(ctts, base + 4)? as i32;
            for _ in 0..run {
                let Some(sample) = samples.get_mut(index) else {
                    break;
                };
                sample.cts_offset = offset;
                index += 1;
            }
        }
    }

    Ok(())
}

fn apply_sync(stbl: &[u8], samples: &mut [Sample]) -> io::Result<()> {
    let Some(stss) = find_box(stbl, b"stss") else {
        // No `stss` means every sample is a sync sample.
        for sample in samples.iter_mut() {
            sample.is_sync = true;
        }
        return Ok(());
    };
    let entries = usize::try_from(u32be(stss, 4)?).unwrap_or(0);
    for entry in 0..entries {
        let number = u32be(stss, 8 + entry * 4)? as usize;
        if let Some(sample) = number.checked_sub(1).and_then(|i| samples.get_mut(i)) {
            sample.is_sync = true;
        }
    }
    Ok(())
}

fn parse_stsz(stbl: &[u8]) -> io::Result<Vec<u32>> {
    if let Some(stsz) = find_box(stbl, b"stsz") {
        let uniform = u32be(stsz, 4)?;
        let count = usize::try_from(u32be(stsz, 8)?).unwrap_or(0);
        if uniform != 0 {
            return Ok(vec![uniform; count]);
        }
        let mut sizes = Vec::with_capacity(count.min(1 << 20));
        for i in 0..count {
            sizes.push(u32be(stsz, 12 + i * 4)?);
        }
        return Ok(sizes);
    }

    // `stz2` packs the sizes into 4, 8 or 16 bit fields.
    let stz2 = find_box(stbl, b"stz2").ok_or_else(|| invalid("no stsz box found"))?;
    let field = *stz2.get(7).ok_or_else(|| invalid("truncated stz2 box"))?;
    let count = usize::try_from(u32be(stz2, 8)?).unwrap_or(0);
    let data = stz2.get(12..).ok_or_else(|| invalid("truncated stz2 box"))?;
    let mut sizes = Vec::with_capacity(count.min(1 << 20));
    for i in 0..count {
        let size = match field {
            4 => {
                let byte = *data.get(i / 2).ok_or_else(|| invalid("truncated stz2 box"))?;
                u32::from(if i % 2 == 0 { byte >> 4 } else { byte & 0x0f })
            }
            8 => u32::from(*data.get(i).ok_or_else(|| invalid("truncated stz2 box"))?),
            16 => u32::from(u16be(data, i * 2)?),
            _ => return Err(invalid("unsupported stz2 field size")),
        };
        sizes.push(size);
    }
    Ok(sizes)
}

fn parse_chunk_offsets(stbl: &[u8]) -> io::Result<Vec<u64>> {
    if let Some(co64) = find_box(stbl, b"co64") {
        let count = usize::try_from(u32be(co64, 4)?).unwrap_or(0);
        let mut offsets = Vec::with_capacity(count.min(1 << 20));
        for i in 0..count {
            offsets.push(u64be(co64, 8 + i * 8)?);
        }
        return Ok(offsets);
    }
    let stco = find_box(stbl, b"stco").ok_or_else(|| invalid("no stco box found"))?;
    let count = usize::try_from(u32be(stco, 4)?).unwrap_or(0);
    let mut offsets = Vec::with_capacity(count.min(1 << 20));
    for i in 0..count {
        offsets.push(u64::from(u32be(stco, 8 + i * 4)?));
    }
    Ok(offsets)
}

// --- box plumbing ------------------------------------------------------------

async fn read_header(file: &mut File, pos: u64) -> io::Result<(u64, [u8; 4], u64)> {
    let mut head = [0u8; 16];
    file.seek(io::SeekFrom::Start(pos)).await?;
    file.read_exact(&mut head[..8]).await?;
    let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let kind = [head[4], head[5], head[6], head[7]];
    if size == 1 {
        file.read_exact(&mut head[8..16]).await?;
        let large = u64::from_be_bytes(head[8..16].try_into().unwrap());
        Ok((large, kind, 16))
    } else {
        Ok((u64::from(size), kind, 8))
    }
}

/// Iterates the direct children of a box body that carry `kind`.
fn boxes<'a>(body: &'a [u8], kind: &'a [u8; 4]) -> impl Iterator<Item = &'a [u8]> {
    children(body).filter_map(move |(k, payload)| (&k == kind).then_some(payload))
}

/// Depth-first search for `kind`, descending only into known container boxes.
fn find_box<'a>(body: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    for (found, payload) in children(body) {
        if &found == kind {
            return Some(payload);
        }
        if CONTAINERS.contains(&&found)
            && let Some(hit) = find_box(payload, kind)
        {
            return Some(hit);
        }
    }
    None
}

fn children(body: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 8 > body.len() {
            return None;
        }
        let size = u32::from_be_bytes(body[pos..pos + 4].try_into().ok()?) as usize;
        let kind: [u8; 4] = body[pos + 4..pos + 8].try_into().ok()?;
        let (size, header) = if size == 1 {
            if pos + 16 > body.len() {
                return None;
            }
            let large = u64::from_be_bytes(body[pos + 8..pos + 16].try_into().ok()?);
            (usize::try_from(large).ok()?, 16)
        } else if size == 0 {
            (body.len() - pos, 8)
        } else {
            (size, 8)
        };
        if size < header || pos + size > body.len() {
            return None;
        }
        let payload = &body[pos + header..pos + size];
        pos += size;
        Some((kind, payload))
    })
}

/// First sample entry of an `stsd` box, as `(format, body)`.
fn stsd_first_entry(stsd: &[u8]) -> Option<([u8; 4], &[u8])> {
    // stsd body: version/flags (4) + entry_count (4), then the entries.
    children(stsd.get(8..)?).next()
}

/// Reads the AAC audio object type out of an `esds` descriptor.
fn audio_object_type(esds: &[u8]) -> Option<u8> {
    // Scan for the DecoderSpecificInfo tag (0x05) rather than walking the
    // variable-length descriptor chain: the payload is a handful of bytes.
    let mut i = 4; // skip version/flags
    while i < esds.len() {
        if esds[i] == 0x05 {
            // Descriptor lengths use up to four 7-bit continuation bytes.
            let mut j = i + 1;
            while j < esds.len() && j < i + 5 && esds[j] & 0x80 != 0 {
                j += 1;
            }
            let aot = esds.get(j + 1)? >> 3;
            return (aot != 0).then_some(aot);
        }
        i += 1;
    }
    None
}

fn version(full_box: &[u8]) -> u8 {
    full_box.first().copied().unwrap_or(0)
}

fn fourcc(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).trim_end().to_string()
}

fn u16be(buf: &[u8], at: usize) -> io::Result<u16> {
    buf.get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or_else(|| invalid("truncated box"))
}

fn u32be(buf: &[u8], at: usize) -> io::Result<u32> {
    buf.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| invalid("truncated box"))
}

fn u64be(buf: &[u8], at: usize) -> io::Result<u64> {
    buf.get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| invalid("truncated box"))
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
