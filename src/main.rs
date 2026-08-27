use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{from_fn_with_state, Next},
    response::Response,
    routing::get,
    Router,
};
use dotenvy::dotenv;
use percent_encoding::percent_decode_str;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

mod api;
mod fmp4;
mod hls;
mod mp4;

use api::StreamApi;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_BIND_ADDR: &str = "0.0.0.0";
const DEFAULT_FILES_DIR: &str = "public";
/// Sub-directory of `FILES_DIR` whose `.mp4` files are exposed as HLS.
const DEFAULT_STREAM_DIR: &str = "stream";
/// Nominal segment length, in seconds. Real segments land on the next keyframe.
const DEFAULT_TARGET_DURATION: f64 = 6.0;

#[tokio::main]
async fn main() -> ExitCode {
    // Load the .env file when there is one, but never require it: in Docker,
    // systemd or Kubernetes the variables come from the real environment.
    if let Err(e) = dotenv() {
        if !e.not_found() {
            eprintln!("Error reading .env file: {e}");
            return ExitCode::FAILURE;
        }
    }

    let port: u16 = match env::var("PORT") {
        Ok(p) => match p.parse() {
            Ok(port) => port,
            Err(e) => {
                eprintln!("Invalid PORT value `{p}`: {e}");
                return ExitCode::FAILURE;
            }
        },
        Err(_) => DEFAULT_PORT,
    };

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| String::from(DEFAULT_BIND_ADDR));
    let bind_addr: IpAddr = match bind_addr.parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("Invalid BIND_ADDR value `{bind_addr}`: {e}");
            return ExitCode::FAILURE;
        }
    };

    let download_dir = env::var("FILES_DIR").unwrap_or_else(|_| String::from(DEFAULT_FILES_DIR));

    // Create dir if not exist
    if let Err(e) = tokio::fs::create_dir_all(&download_dir).await {
        eprintln!("Error creating directory `{download_dir}`: {e}");
        return ExitCode::FAILURE;
    }

    // Resolved once at startup so every request can be checked against it.
    let base_dir = match tokio::fs::canonicalize(&download_dir).await {
        Ok(dir) => Arc::new(dir),
        Err(e) => {
            eprintln!("Error resolving directory `{download_dir}`: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stream_dir = env::var("STREAM_DIR").unwrap_or_else(|_| String::from(DEFAULT_STREAM_DIR));
    let target_duration = match env::var("HLS_TARGET_DURATION") {
        Ok(v) => match v.parse::<f64>() {
            Ok(d) if d.is_finite() && d > 0.0 => d,
            _ => {
                eprintln!("Invalid HLS_TARGET_DURATION value `{v}`: expected a positive number");
                return ExitCode::FAILURE;
            }
        },
        Err(_) => DEFAULT_TARGET_DURATION,
    };

    // Chunking and playlists are served from the same tree as the raw files, so
    // a client can fall back to a plain download of the source .mp4.
    let stream_api = Arc::new(StreamApi::new(base_dir.join(&stream_dir), target_duration));

    let app = Router::new()
        // Base route
        .route("/", get(|| async { "Hello World!" }))
        // HLS packaging API
        .merge(stream_api.router())
        // Serve the static files
        .fallback_service(ServeDir::new(base_dir.as_path()))
        .layer(from_fn_with_state(base_dir.clone(), reject_escaping_paths))
        .layer(CompressionLayer::new());

    // Launching server
    let addr = SocketAddr::from((bind_addr, port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("Error binding to {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("Listening on {addr}, serving {}", base_dir.display());
    println!("HLS API on /api/stream, packaging {}", base_dir.join(&stream_dir).display());
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// `ServeDir` blocks `..` traversal but still follows symbolic links, so a link
/// dropped inside the served directory can hand out files from anywhere on the
/// host. Resolve the target first and refuse anything landing outside the base.
async fn reject_escaping_paths(
    State(base_dir): State<Arc<PathBuf>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let requested = req.uri().path();

    let decoded = percent_decode_str(requested.trim_start_matches('/'))
        .decode_utf8()
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut path = base_dir.as_path().to_path_buf();
    for component in Path::new(decoded.as_ref()).components() {
        match component {
            Component::Normal(comp) => path.push(comp),
            Component::CurDir => {}
            // Same rejection set as ServeDir, applied before we touch the disk.
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(StatusCode::NOT_FOUND);
            }
        }
    }

    match tokio::fs::canonicalize(&path).await {
        // Outside the served directory: a symlink pointing elsewhere.
        Ok(resolved) if !resolved.starts_with(base_dir.as_path()) => Err(StatusCode::NOT_FOUND),
        Ok(_) => Ok(next.run(req).await),
        // Missing file: let ServeDir produce its own 404.
        Err(_) => Ok(next.run(req).await),
    }
}
