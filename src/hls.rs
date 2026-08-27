//! HLS packaging on top of the demuxer and the fMP4 writer.
//!
//! A source MP4 is cut into segments at video keyframes, described by an M3U8
//! playlist, and each segment is remuxed into CMAF on demand. Nothing is
//! written to disk: the only state kept is the parsed sample table.

use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::fmp4::{self, TrackRun};
use crate::mp4::{self, Mp4, Sample, TrackKind};

/// Upper bound on the bytes one segment may pull into memory.
const MAX_SEGMENT_BYTES: u64 = 128 << 20;

/// One segment: where each track's samples start and stop.
pub struct Segment {
    pub duration: f64,
    /// `(track index, sample range)`, in track order. Empty ranges are dropped.
    pub runs: Vec<(usize, Range<usize>)>,
}

/// A source file, parsed and cut into segments.
pub struct Stream {
    pub path: PathBuf,
    pub mp4: Mp4,
    pub init: Vec<u8>,
    pub segments: Vec<Segment>,
    /// Identity of the file this was built from, to notice replacements.
    revision: (u64, Option<SystemTime>),
}

impl Stream {
    /// Longest segment, rounded up, for `EXT-X-TARGETDURATION`.
    fn target_duration(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| s.duration.ceil() as u64)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Average bitrate in bits per second, for `BANDWIDTH`.
    fn bandwidth(&self) -> u64 {
        let bytes: u64 = self
            .mp4
            .tracks
            .iter()
            .flat_map(|t| t.samples.iter())
            .map(|s| u64::from(s.size))
            .sum();
        let duration = self.mp4.duration();
        if duration <= 0.0 {
            return 0;
        }
        (bytes as f64 * 8.0 / duration) as u64
    }

    /// Multivariant playlist. A remux exposes exactly one rendition, but players
    /// and CDNs still expect this entry point.
    pub fn master_playlist(&self, base: &str) -> String {
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
        out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={}", self.bandwidth()));
        if let Some(video) = self.mp4.video() {
            // tkhd stores the display size as 16.16 fixed point.
            let (w, h) = (video.width >> 16, video.height >> 16);
            if w > 0 && h > 0 {
                out.push_str(&format!(",RESOLUTION={w}x{h}"));
            }
        }
        let codecs: Vec<String> = self.mp4.tracks.iter().filter_map(|t| t.codec()).collect();
        if !codecs.is_empty() {
            out.push_str(&format!(",CODECS=\"{}\"", codecs.join(",")));
        }
        out.push_str(&format!("\n{base}/index.m3u8\n"));
        out
    }

    /// Media playlist listing every segment of the (VOD) stream.
    pub fn media_playlist(&self, base: &str) -> String {
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
        out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        out.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.target_duration()
        ));
        out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
        out.push_str(&format!("#EXT-X-MAP:URI=\"{base}/init.mp4\"\n"));
        for (index, segment) in self.segments.iter().enumerate() {
            out.push_str(&format!("#EXTINF:{:.6},\n", segment.duration));
            out.push_str(&format!("{base}/segment-{index}.m4s\n"));
        }
        out.push_str("#EXT-X-ENDLIST\n");
        out
    }

    /// Remuxes segment `index` into a standalone CMAF segment.
    pub async fn segment(&self, index: usize) -> io::Result<Option<Vec<u8>>> {
        let Some(segment) = self.segments.get(index) else {
            return Ok(None);
        };

        let runs: Vec<(&[Sample], &mp4::Track)> = segment
            .runs
            .iter()
            .filter_map(|(track, range)| {
                let track = self.mp4.tracks.get(*track)?;
                Some((track.samples.get(range.clone())?, track))
            })
            .collect();

        let total: u64 = runs.iter().map(|(s, _)| fmp4::run_data_len(s)).sum();
        if total > MAX_SEGMENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "segment is too large to remux",
            ));
        }

        let mut file = File::open(&self.path).await?;
        let mut data = Vec::with_capacity(total as usize);
        for (samples, _) in &runs {
            read_samples(&mut file, samples, &mut data).await?;
        }

        let runs: Vec<TrackRun<'_>> = runs
            .iter()
            .map(|&(samples, track)| TrackRun { track, samples })
            .collect();

        // Sequence numbers are 1-based so that segment 0 is still a valid `moof`.
        Ok(Some(fmp4::media_segment(index as u32 + 1, &runs, &data)))
    }
}

/// Copies the bytes of `samples` out of `file`, merging adjacent samples into a
/// single read: tracks are interleaved on disk, so a run is a handful of
/// contiguous spans rather than one.
async fn read_samples(file: &mut File, samples: &[Sample], out: &mut Vec<u8>) -> io::Result<()> {
    let mut span: Option<(u64, u64)> = None;
    for sample in samples {
        span = match span {
            Some((start, len)) if start + len == sample.offset => {
                Some((start, len + u64::from(sample.size)))
            }
            Some((start, len)) => {
                read_span(file, start, len, out).await?;
                Some((sample.offset, u64::from(sample.size)))
            }
            None => Some((sample.offset, u64::from(sample.size))),
        };
    }
    if let Some((start, len)) = span {
        read_span(file, start, len, out).await?;
    }
    Ok(())
}

async fn read_span(file: &mut File, start: u64, len: u64, out: &mut Vec<u8>) -> io::Result<()> {
    let len = usize::try_from(len).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "sample span is too large")
    })?;
    file.seek(io::SeekFrom::Start(start)).await?;
    let at = out.len();
    out.resize(at + len, 0);
    file.read_exact(&mut out[at..]).await?;
    Ok(())
}

/// Cuts the movie into segments of roughly `target` seconds.
///
/// Video is cut only at keyframes, so real durations drift towards the source's
/// GOP length; every other track follows those same wall-clock boundaries.
fn plan_segments(mp4: &Mp4, target: f64) -> Vec<Segment> {
    // Video drives the cuts. Without a video track any sample is a valid start.
    let reference = mp4
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Video)
        .unwrap_or(0);
    let track = &mp4.tracks[reference];
    let timescale = f64::from(track.timescale);

    let mut times = vec![0.0f64];
    let mut last = 0.0f64;
    for sample in track.samples.iter().skip(1) {
        if !sample.is_sync {
            continue;
        }
        let time = sample.dts as f64 / timescale;
        if time - last >= target {
            times.push(time);
            last = time;
        }
    }

    // Per track, translate the cut times into sample indices. Doing it for the
    // reference track too keeps a single code path.
    let bounds: Vec<Vec<usize>> = mp4
        .tracks
        .iter()
        .map(|track| {
            let mut indices: Vec<usize> = times
                .iter()
                .map(|time| track.sample_at((time * f64::from(track.timescale)) as u64))
                .collect();
            indices.push(track.samples.len());
            indices
        })
        .collect();

    let duration = mp4.duration();
    (0..times.len())
        .map(|index| {
            let end = times.get(index + 1).copied().unwrap_or(duration);
            let runs = bounds
                .iter()
                .enumerate()
                .filter_map(|(track, indices)| {
                    let range = indices[index]..indices[index + 1];
                    (!range.is_empty()).then_some((track, range))
                })
                .collect();
            Segment {
                duration: (end - times[index]).max(0.0),
                runs,
            }
        })
        .collect()
}

/// Parsed streams, keyed by name, invalidated when the file changes on disk.
pub struct Cache {
    target_duration: f64,
    entries: Mutex<HashMap<String, Arc<Stream>>>,
}

impl Cache {
    pub fn new(target_duration: f64) -> Self {
        Self {
            target_duration,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the packaged stream for `path`, parsing it on first use.
    pub async fn get(&self, name: &str, path: &Path) -> io::Result<Arc<Stream>> {
        let revision = revision(path).await?;
        if let Some(stream) = self.lookup(name)
            && stream.revision == revision
        {
            return Ok(stream);
        }

        let mut file = File::open(path).await?;
        let mp4 = mp4::parse(&mut file).await?;
        let stream = Arc::new(Stream {
            path: path.to_path_buf(),
            init: fmp4::init_segment(&mp4),
            segments: plan_segments(&mp4, self.target_duration),
            mp4,
            revision,
        });

        // Two requests racing on a cold entry both parse; the loser's work is
        // simply dropped, which is cheaper than holding a lock across the parse.
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(name.to_string(), stream.clone());
        }
        Ok(stream)
    }

    fn lookup(&self, name: &str) -> Option<Arc<Stream>> {
        self.entries.lock().ok()?.get(name).cloned()
    }
}

/// Cheap identity of a file: size plus modification time.
async fn revision(path: &Path) -> io::Result<(u64, Option<SystemTime>)> {
    let meta = tokio::fs::metadata(path).await?;
    Ok((meta.len(), meta.modified().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::Track;

    /// A track of `count` samples, each `duration` long, with a sync sample
    /// every `gop` samples (`gop` of 0 marks every sample as sync).
    fn track(id: u32, kind: TrackKind, timescale: u32, count: usize, duration: u32, gop: usize) -> Track {
        let samples = (0..count)
            .map(|i| Sample {
                offset: i as u64 * 10,
                size: 10,
                dts: i as u64 * u64::from(duration),
                duration,
                cts_offset: 0,
                is_sync: gop == 0 || i % gop == 0,
            })
            .collect::<Vec<_>>();
        Track {
            id,
            kind,
            timescale,
            duration: count as u64 * u64::from(duration),
            width: 0,
            height: 0,
            language: 0x55C4,
            stsd: Vec::new(),
            samples,
        }
    }

    /// Every sample of every track must land in exactly one segment, in order.
    fn assert_partitions(mp4: &Mp4, segments: &[Segment]) {
        for (index, track) in mp4.tracks.iter().enumerate() {
            let mut next = 0;
            for segment in segments {
                if let Some((_, range)) = segment.runs.iter().find(|(t, _)| *t == index) {
                    assert_eq!(range.start, next, "gap or overlap in track {index}");
                    next = range.end;
                }
            }
            assert_eq!(next, track.samples.len(), "track {index} truncated");
        }
    }

    #[test]
    fn segments_start_on_keyframes_and_cover_every_sample() {
        // 30 frames at 10 fps (3 s), keyframe every 10 frames.
        let mp4 = Mp4 {
            timescale: 1000,
            tracks: vec![track(1, TrackKind::Video, 1000, 30, 100, 10)],
        };
        let segments = plan_segments(&mp4, 1.0);

        assert_eq!(segments.len(), 3);
        assert_partitions(&mp4, &segments);
        for segment in &segments {
            let (_, range) = &segment.runs[0];
            assert!(
                mp4.tracks[0].samples[range.start].is_sync,
                "segment does not start on a keyframe"
            );
            assert_eq!(range.len(), 10);
        }
    }

    #[test]
    fn a_target_shorter_than_the_gop_still_cuts_only_on_keyframes() {
        let mp4 = Mp4 {
            timescale: 1000,
            tracks: vec![track(1, TrackKind::Video, 1000, 30, 100, 10)],
        };
        // Asking for 0.1 s cannot produce more segments than there are keyframes.
        let segments = plan_segments(&mp4, 0.1);
        assert_eq!(segments.len(), 3);
        assert_partitions(&mp4, &segments);
    }

    #[test]
    fn audio_follows_the_video_cut_points() {
        // Video: 3 s at 10 fps, keyframes every second. Audio: 48 kHz, 1024
        // samples per frame, so its frames never align with the video ones.
        let mp4 = Mp4 {
            timescale: 1000,
            tracks: vec![
                track(1, TrackKind::Video, 1000, 30, 100, 10),
                track(2, TrackKind::Audio, 48000, 141, 1024, 0),
            ],
        };
        let segments = plan_segments(&mp4, 1.0);

        assert_eq!(segments.len(), 3);
        assert_partitions(&mp4, &segments);
        // First audio sample of segment 1 is the first one at or after 1 s.
        let (_, audio) = segments[1]
            .runs
            .iter()
            .find(|(t, _)| *t == 1)
            .expect("audio run");
        assert_eq!(audio.start, 47);
        assert!(mp4.tracks[1].samples[47].dts >= 48000);
        assert!(mp4.tracks[1].samples[46].dts < 48000);
    }

    #[test]
    fn an_audio_only_movie_is_cut_on_the_target() {
        let mp4 = Mp4 {
            timescale: 1000,
            tracks: vec![track(1, TrackKind::Audio, 48000, 141, 1024, 0)],
        };
        let segments = plan_segments(&mp4, 1.0);
        assert_eq!(segments.len(), 3);
        assert_partitions(&mp4, &segments);
    }

    #[test]
    fn a_single_sample_movie_yields_one_segment() {
        let mp4 = Mp4 {
            timescale: 1000,
            tracks: vec![track(1, TrackKind::Video, 1000, 1, 100, 10)],
        };
        let segments = plan_segments(&mp4, 6.0);
        assert_eq!(segments.len(), 1);
        assert_partitions(&mp4, &segments);
    }
}
