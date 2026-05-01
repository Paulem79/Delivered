use std::env;
use axum::{
    routing::get,
    Router
};
use tower_http::services::ServeDir;
use std::net::SocketAddr;
use dotenvy::dotenv;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    // load environment variables from .env file
    dotenv().expect(".env file not found");

    let port = env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(3000);

    let download_dir = env::var("FILES_DIR")
        .ok()
        .unwrap_or_else(|| String::from("public"));

    // Create dir if not exist
    if let Err(e) = tokio::fs::create_dir_all(&download_dir).await {
        eprintln!("Error creating directory : {}", e);
    }

    let app = Router::new()
        // Base route
        .route("/", get(|| async { "Hello World!" }))
        // Serve the static files
        .fallback_service(ServeDir::new(&download_dir));

    // Launching server
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    println!("Listening to port {}", format!("{}", port.to_string()));
    axum::serve(listener, app).await.unwrap();
}