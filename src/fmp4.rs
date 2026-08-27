//! Fragmented MP4 (CMAF) writer.
//!
//! Builds the two things an HLS player asks for: an *init segment* carrying the
//! movie header with empty sample tables, and *media segments* pairing a `moof`
//! index with the raw sample bytes copied out of the source file. The elementary
//! streams are never touched, so this is a remux, not a transcode.

use crate::mp4::{Mp4, Sample, Track, TrackKind};

/// `sample_flags` for a keyframe: depends-on-others = 2 ("I-picture").
const FLAGS_SYNC: u32 = 0x0200_0000;
/// Non-keyframe: depends-on-others = 1, and the non-sync bit set.
const FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// A contiguous run of samples of one track, as scheduled into one segment.
pub struct TrackRun<'a> {
    pub track: &'a Track,
    pub samples: &'a [Sample],
}

/// Builds the init segment (`ftyp` + `moov`) describing every track.
pub fn init_segment(mp4: &Mp4) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);

    out.extend_from_slice(&boxed(b"ftyp", &{
        let mut b = Vec::new();
        b.extend_from_slice(b"iso5");
        b.extend_from_slice(&0u32.to_be_bytes()); // minor_version
        for brand in [b"iso5", b"iso6", b"mp41", b"avc1", b"cmfc"] {
            b.extend_from_slice(brand);
        }
        b
    }));

    let mut moov = Vec::new();
    moov.extend_from_slice(&boxed(b"mvhd", &mvhd(mp4)));
    for track in &mp4.tracks {
        moov.extend_from_slice(&boxed(b"trak", &trak(track)));
    }
    moov.extend_from_slice(&boxed(b"mvex", &{
        let mut b = Vec::new();
        for track in &mp4.tracks {
            b.extend_from_slice(&boxed(b"trex", &trex(track)));
        }
        b
    }));
    out.extend_from_slice(&boxed(b"moov", &moov));
    out
}

/// Builds one media segment (`styp` + `moof` + `mdat`).
///
/// `data` must hold every run's sample bytes concatenated in run order, which is
/// the layout the patched `trun.data_offset` fields describe.
pub fn media_segment(sequence: u32, runs: &[TrackRun<'_>], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1024);

    out.extend_from_slice(&boxed(b"styp", &{
        let mut b = Vec::new();
        b.extend_from_slice(b"msdh");
        b.extend_from_slice(&0u32.to_be_bytes());
        for brand in [b"msdh", b"msix", b"cmfs"] {
            b.extend_from_slice(brand);
        }
        b
    }));

    // Each `trun` states where its samples start relative to the `moof`, which
    // is only known once the whole `moof` is laid out. Note the field positions
    // on the way out and patch them afterwards.
    let mut moof = Vec::new();
    let mut patches = Vec::with_capacity(runs.len());
    moof.extend_from_slice(&boxed(b"mfhd", &{
        let mut b = vec![0, 0, 0, 0];
        b.extend_from_slice(&sequence.to_be_bytes());
        b
    }));
    for run in runs {
        let (traf, offset_field) = traf(run);
        // +8 for the `traf` box header the payload is about to be wrapped in.
        patches.push(moof.len() + 8 + offset_field);
        moof.extend_from_slice(&boxed(b"traf", &traf));
    }
    let mut moof = boxed(b"moof", &moof);

    // Sample data starts right after the `mdat` header, and each run follows the
    // previous one; `moof.len()` already accounts for the `moof` header itself.
    let mut data_offset = moof.len() as u32 + 8;
    for (patch, run) in patches.iter().zip(runs) {
        // +8 to skip the `moof` header that `boxed` prepended to every position.
        let at = patch + 8;
        moof[at..at + 4].copy_from_slice(&data_offset.to_be_bytes());
        data_offset += run_data_len(run.samples) as u32;
    }

    out.extend_from_slice(&moof);
    out.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(data);
    out
}

/// Total sample bytes a run contributes to the `mdat`.
pub fn run_data_len(samples: &[Sample]) -> u64 {
    samples.iter().map(|s| u64::from(s.size)).sum()
}

// --- moov ---------------------------------------------------------------

fn mvhd(mp4: &Mp4) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0]; // version 0, flags 0
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&mp4.timescale.to_be_bytes());
    // Duration is unknown to a fragmented movie header; `mehd` is optional and
    // players take the total from the playlist instead.
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&UNITY_MATRIX);
    b.extend_from_slice(&[0u8; 24]); // pre_defined
    let next_track = mp4.tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    b.extend_from_slice(&next_track.to_be_bytes());
    b
}

fn trak(track: &Track) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&boxed(b"tkhd", &tkhd(track)));
    b.extend_from_slice(&boxed(b"mdia", &mdia(track)));
    b
}

fn tkhd(track: &Track) -> Vec<u8> {
    // flags 3 = track_enabled | track_in_movie
    let mut b = vec![0, 0, 0, 3];
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&track.id.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&0u16.to_be_bytes()); // layer
    b.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    let volume: u16 = if track.kind == TrackKind::Audio {
        0x0100
    } else {
        0
    };
    b.extend_from_slice(&volume.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&UNITY_MATRIX);
    b.extend_from_slice(&track.width.to_be_bytes());
    b.extend_from_slice(&track.height.to_be_bytes());
    b
}

fn mdia(track: &Track) -> Vec<u8> {
    let mut b = Vec::new();

    let mut mdhd = vec![0, 0, 0, 0];
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    mdhd.extend_from_slice(&track.timescale.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    mdhd.extend_from_slice(&track.language.to_be_bytes());
    mdhd.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    b.extend_from_slice(&boxed(b"mdhd", &mdhd));

    let (handler, name) = match track.kind {
        TrackKind::Audio => (b"soun", "SoundHandler"),
        _ => (b"vide", "VideoHandler"),
    };
    let mut hdlr = vec![0, 0, 0, 0];
    hdlr.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    hdlr.extend_from_slice(handler);
    hdlr.extend_from_slice(&[0u8; 12]); // reserved
    hdlr.extend_from_slice(name.as_bytes());
    hdlr.push(0);
    b.extend_from_slice(&boxed(b"hdlr", &hdlr));

    b.extend_from_slice(&boxed(b"minf", &minf(track)));
    b
}

fn minf(track: &Track) -> Vec<u8> {
    let mut b = Vec::new();
    match track.kind {
        TrackKind::Audio => b.extend_from_slice(&boxed(b"smhd", &[0, 0, 0, 0, 0, 0, 0, 0])),
        // vmhd: version 0, flags 1, graphicsmode 0, opcolor {0,0,0}
        _ => b.extend_from_slice(&boxed(b"vmhd", &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0])),
    }

    // dinf/dref with a single self-contained entry (flags 1 = data in this file).
    let url = boxed(b"url ", &[0, 0, 0, 1]);
    let mut dref = vec![0, 0, 0, 0];
    dref.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    dref.extend_from_slice(&url);
    b.extend_from_slice(&boxed(b"dinf", &boxed(b"dref", &dref)));

    // The sample tables must be present but empty: the timing lives in `trun`.
    let mut stbl = Vec::new();
    stbl.extend_from_slice(&boxed(b"stsd", &track.stsd));
    stbl.extend_from_slice(&boxed(b"stts", &[0, 0, 0, 0, 0, 0, 0, 0]));
    stbl.extend_from_slice(&boxed(b"stsc", &[0, 0, 0, 0, 0, 0, 0, 0]));
    // stsz also carries a zero sample_size before the zero count.
    stbl.extend_from_slice(&boxed(b"stsz", &[0; 12]));
    stbl.extend_from_slice(&boxed(b"stco", &[0, 0, 0, 0, 0, 0, 0, 0]));
    b.extend_from_slice(&boxed(b"stbl", &stbl));
    b
}

fn trex(track: &Track) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0];
    b.extend_from_slice(&track.id.to_be_bytes());
    b.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration
    b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
    b.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
    b
}

// --- moof ---------------------------------------------------------------

/// Returns the `traf` payload and the offset, within it, of the `trun`
/// `data_offset` field that the caller has to patch.
fn traf(run: &TrackRun<'_>) -> (Vec<u8>, usize) {
    let mut b = Vec::new();

    // tfhd flags: default-base-is-moof (0x020000) makes sample offsets relative
    // to the enclosing `moof`, which is what `trun.data_offset` below assumes.
    let mut tfhd = vec![0x00, 0x02, 0x00, 0x00];
    tfhd.extend_from_slice(&run.track.id.to_be_bytes());
    b.extend_from_slice(&boxed(b"tfhd", &tfhd));

    // tfdt version 1: 64 bit baseMediaDecodeTime, in the track timescale.
    let base_dts = run.samples.first().map_or(0, |s| s.dts);
    let mut tfdt = vec![1, 0, 0, 0];
    tfdt.extend_from_slice(&base_dts.to_be_bytes());
    b.extend_from_slice(&boxed(b"tfdt", &tfdt));

    // trun version 1 (signed composition offsets), with per-sample duration,
    // size, flags and composition offset all present.
    let mut trun = vec![1, 0x00, 0x0f, 0x01];
    trun.extend_from_slice(&(run.samples.len() as u32).to_be_bytes());
    let offset_field = 8 + trun.len(); // +8 for the `trun` box header
    trun.extend_from_slice(&0u32.to_be_bytes()); // data_offset, patched later
    for sample in run.samples {
        trun.extend_from_slice(&sample.duration.to_be_bytes());
        trun.extend_from_slice(&sample.size.to_be_bytes());
        let flags = if sample.is_sync {
            FLAGS_SYNC
        } else {
            FLAGS_NON_SYNC
        };
        trun.extend_from_slice(&flags.to_be_bytes());
        trun.extend_from_slice(&sample.cts_offset.to_be_bytes());
    }
    let offset_field = b.len() + offset_field;
    b.extend_from_slice(&boxed(b"trun", &trun));

    (b, offset_field)
}

// --- box plumbing -------------------------------------------------------

const UNITY_MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x00, 0x00, 0x00,
];

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::{Mp4, Sample, Track, TrackKind};

    fn sample(offset: u64, size: u32, dts: u64, is_sync: bool) -> Sample {
        Sample {
            offset,
            size,
            dts,
            duration: 100,
            cts_offset: 0,
            is_sync,
        }
    }

    fn track(id: u32, kind: TrackKind, samples: Vec<Sample>) -> Track {
        Track {
            id,
            kind,
            timescale: 1000,
            duration: samples.len() as u64 * 100,
            width: 320 << 16,
            height: 240 << 16,
            language: 0x55C4,
            // Smallest plausible stsd: version/flags, entry_count, one entry.
            stsd: vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 8, b'a', b'v', b'c', b'1'],
            samples,
        }
    }

    /// Walks the top-level boxes, returning `(kind, offset, size)`.
    fn top_level(buf: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = buf[off + 4..off + 8].try_into().unwrap();
            out.push((kind, off, size));
            off += size;
        }
        out
    }

    #[test]
    fn init_segment_is_a_well_formed_ftyp_moov_pair() {
        let mp4 = Mp4 {
            timescale: 1000,
            tracks: vec![
                track(1, TrackKind::Video, vec![sample(0, 10, 0, true)]),
                track(2, TrackKind::Audio, vec![sample(10, 20, 0, true)]),
            ],
        };
        let init = init_segment(&mp4);
        let boxes = top_level(&init);
        assert_eq!(
            boxes.iter().map(|b| b.0).collect::<Vec<_>>(),
            vec![*b"ftyp", *b"moov"]
        );
        // Every byte must be accounted for by a top-level box.
        assert_eq!(boxes.iter().map(|b| b.2).sum::<usize>(), init.len());
    }

    #[test]
    fn trun_data_offsets_point_at_each_run() {
        let video = track(
            1,
            TrackKind::Video,
            vec![sample(0, 4, 0, true), sample(4, 6, 100, false)],
        );
        let audio = track(2, TrackKind::Audio, vec![sample(10, 5, 0, true)]);

        // 4 + 6 video bytes, then 5 audio bytes, in run order.
        let data: Vec<u8> = (0u8..15).collect();
        let runs = vec![
            TrackRun {
                track: &video,
                samples: &video.samples,
            },
            TrackRun {
                track: &audio,
                samples: &audio.samples,
            },
        ];
        let segment = media_segment(1, &runs, &data);

        let boxes = top_level(&segment);
        assert_eq!(
            boxes.iter().map(|b| b.0).collect::<Vec<_>>(),
            vec![*b"styp", *b"moof", *b"mdat"]
        );
        assert_eq!(boxes.iter().map(|b| b.2).sum::<usize>(), segment.len());

        let (_, moof_at, moof_size) = boxes[1];
        let (_, mdat_at, _) = boxes[2];
        // The first run must start right after the mdat header, the second
        // exactly 10 bytes further in.
        let offsets = trun_data_offsets(&segment[moof_at..moof_at + moof_size]);
        assert_eq!(offsets.len(), 2);
        assert_eq!(moof_at + offsets[0] as usize, mdat_at + 8);
        assert_eq!(moof_at + offsets[1] as usize, mdat_at + 8 + 10);
        assert_eq!(&segment[mdat_at + 8..], &data[..]);
    }

    /// Reads every `trun.data_offset` out of a `moof`, in order.
    fn trun_data_offsets(moof: &[u8]) -> Vec<i32> {
        let mut out = Vec::new();
        // The payload holds no sample data, so a plain scan cannot false-match.
        let mut at = 0;
        while at + 20 <= moof.len() {
            if &moof[at..at + 4] == b"trun" {
                out.push(i32::from_be_bytes(
                    moof[at + 12..at + 16].try_into().unwrap(),
                ));
            }
            at += 1;
        }
        out
    }
}
