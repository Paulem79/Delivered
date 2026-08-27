//! HTTP surface for the HLS packager.
//!
//! ```text
//! GET /api/stream                          list of available streams (JSON)
//! GET /api/stream/{name}/master.m3u8       multivariant playlist
//! GET /api/stream/{name}/index.m3u8        media playlist
//! GET /api/stream/{name}/init.mp4          CMAF init segment
//! GET /api/stream/{name}/segment-{n}.m4s   CMAF media segment
//! ```
//!
//! `.m3u` is accepted everywhere `.m3u8` is, for players that key off the
//! extension.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as UrlPath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::hls::{Cache, Stream};

/// Source extension we package. Anything else in the directory is ignored.
const SOURCE_EXT: &str = "mp4";

const PLAYLIST_TYPE: &str = "application/vnd.apple.mpegurl";
const SEGMENT_TYPE: &str = "video/iso.segment";
const INIT_TYPE: &str = "video/mp4";

/// Playlists are cheap to rebuild and follow the file; segments never change
/// for a given revision, so they can sit in a cache for a long time.
const PLAYLIST_CACHE: &str = "public, max-age=10";
const SEGMENT_CACHE: &str = "public, max-age=31536000, immutable";

pub struct StreamApi {
    /// Directory holding the source `.mp4` files.
    dir: PathBuf,
    cache: Cache,
}

impl StreamApi {
    pub fn new(dir: PathBuf, target_duration: f64) -> Self {
        Self {
            dir,
            cache: Cache::new(target_duration),
        }
    }

    /// Mounts the API under `/api/stream`.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/api/stream", get(list))
            .route("/api/stream/{name}/{file}", get(resource))
            .with_state(self)
    }

    /// Resolves a stream name to its source file, refusing anything that is not
    /// a plain file name directly inside the stream directory.
    fn source(&self, name: &str) -> Option<PathBuf> {
        let name = name.strip_suffix(".mp4").unwrap_or(name);
        if name.is_empty()
            || name.starts_with('.')
            || name.contains(['/', '\\', '\0'])
            || name.contains("..")
        {
            return None;
        }
        Some(self.dir.join(format!("{name}.{SOURCE_EXT}")))
    }

    async fn stream(&self, name: &str) -> Result<Arc<Stream>, Response> {
        let path = self.source(name).ok_or_else(not_found)?;
        self.cache.get(name, &path).await.map_err(into_response)
    }
}

/// `GET /api/stream` — every packageable file in the stream directory.
async fn list(State(api): State<Arc<StreamApi>>) -> Response {
    let mut names = match read_names(&api.dir).await {
        Ok(names) => names,
        Err(e) => return into_response(e),
    };
    names.sort();

    let mut body = String::from("{\"streams\":[");
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        body.push_str("{\"name\":");
        push_json_string(&mut body, name);
        body.push_str(",\"master\":");
        push_json_string(&mut body, &format!("/api/stream/{name}/master.m3u8"));
        body.push('}');
    }
    body.push_str("]}");

    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, PLAYLIST_CACHE),
        ],
        body,
    )
        .into_response()
}

/// `GET /api/stream/{name}/{file}` — playlists, init segment and segments.
async fn resource(
    State(api): State<Arc<StreamApi>>,
    UrlPath((name, file)): UrlPath<(String, String)>,
) -> Response {
    let stream = match api.stream(&name).await {
        Ok(stream) => stream,
        Err(response) => return response,
    };
    // Playlists reference siblings by absolute path so they survive being
    // fetched through a proxy or saved by a downloader.
    let base = format!("/api/stream/{name}");

    let playlist = file.strip_suffix(".m3u8").or_else(|| file.strip_suffix(".m3u"));
    match (playlist, file.as_str()) {
        (Some("master"), _) => playlist_response(stream.master_playlist(&base)),
        (Some("index") | Some("playlist"), _) => playlist_response(stream.media_playlist(&base)),
        (_, "init.mp4") => (
            [
                (header::CONTENT_TYPE, INIT_TYPE),
                (header::CACHE_CONTROL, SEGMENT_CACHE),
            ],
            stream.init.clone(),
        )
            .into_response(),
        _ => match segment_index(&file) {
            Some(index) => match stream.segment(index).await {
                Ok(Some(bytes)) => (
                    [
                        (header::CONTENT_TYPE, SEGMENT_TYPE),
                        (header::CACHE_CONTROL, SEGMENT_CACHE),
                    ],
                    bytes,
                )
                    .into_response(),
                Ok(None) => not_found(),
                Err(e) => into_response(e),
            },
            None => not_found(),
        },
    }
}

/// Parses `segment-{n}.m4s`.
fn segment_index(file: &str) -> Option<usize> {
    file.strip_prefix("segment-")?
        .strip_suffix(".m4s")?
        .parse()
        .ok()
}

fn playlist_response(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, PLAYLIST_TYPE),
            (header::CACHE_CONTROL, PLAYLIST_CACHE),
        ],
        body,
    )
        .into_response()
}

/// Lists the stream names (file stems) in `dir`.
async fn read_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        // An absent directory is an empty catalogue, not a server error.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let is_source = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(SOURCE_EXT));
        if !is_source || !entry.file_type().await?.is_file() {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            names.push(stem.to_string());
        }
    }
    Ok(names)
}

/// Maps a packaging failure onto a status code. A file that is missing or not a
/// usable MP4 is a client error; anything else is ours.
fn into_response(e: io::Error) -> Response {
    match e.kind() {
        io::ErrorKind::NotFound => not_found(),
        io::ErrorKind::InvalidData => {
            (StatusCode::UNPROCESSABLE_ENTITY, format!("{e}\n")).into_response()
        }
        _ => {
            eprintln!("stream error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
        }
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> StreamApi {
        StreamApi::new(PathBuf::from("/files/stream"), 6.0)
    }

    #[test]
    fn a_plain_name_resolves_inside_the_stream_directory() {
        let api = api();
        assert_eq!(
            api.source("clip"),
            Some(PathBuf::from("/files/stream").join("clip.mp4"))
        );
        // The extension is optional, so a client may echo back the file name.
        assert_eq!(api.source("clip.mp4"), api.source("clip"));
    }

    #[test]
    fn names_that_could_escape_the_directory_are_refused() {
        let api = api();
        for name in [
            "",
            "..",
            "../secret",
            "..\\secret",
            "a/../../secret",
            "sub/clip",
            "sub\\clip",
            ".env",
            ".",
            "clip\0.mp4",
        ] {
            assert!(api.source(name).is_none(), "accepted {name:?}");
        }
    }

    #[test]
    fn segment_names_parse_only_in_the_documented_form() {
        assert_eq!(segment_index("segment-0.m4s"), Some(0));
        assert_eq!(segment_index("segment-42.m4s"), Some(42));
        for name in [
            "segment-.m4s",
            "segment-1.mp4",
            "segment--1.m4s",
            "seg-1.m4s",
            "segment-1",
            "init.mp4",
        ] {
            assert_eq!(segment_index(name), None, "accepted {name:?}");
        }
    }

    #[test]
    fn json_strings_escape_what_would_break_the_document() {
        let mut out = String::new();
        push_json_string(&mut out, "a\"b\\c\nd\te");
        assert_eq!(out, r#""a\"b\\c\nd\te""#);

        // Bare control characters are illegal in JSON and must come out escaped.
        let mut out = String::new();
        push_json_string(&mut out, "bell\u{7}");
        assert_eq!(out, r#""bell\u0007""#);
    }
}
