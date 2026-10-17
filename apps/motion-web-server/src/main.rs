//! Bundler and HTTP server for the MotionCraft Studio Web Application (`motion-web`).
//!
//! Usage:
//! - `cargo run -p motion-web-server` — builds `motion-web` (`wasm32-unknown-unknown`),
//!   runs `wasm-bindgen` into `target/web/dist`, copies `index.html`, and serves on `http://127.0.0.1:8080`.
//! - `cargo run -p motion-web-server -- --build-only` — builds `target/web/dist` and exits.
//! - `cargo run -p motion-web-server -- --serve-only` — serves `target/web/dist` without rebuilding.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn build_web_bundle(root: &Path, release: bool) -> Result<PathBuf, String> {
    let profile = if release { "release" } else { "dev" };
    let profile_dir = if release { "release" } else { "debug" };

    println!("==> Building motion-web (wasm32-unknown-unknown, profile={profile})...");
    let status = Command::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            "motion-web",
            "--profile",
            profile,
        ])
        .status()
        .map_err(|e| format!("Failed to invoke cargo build: {e}"))?;

    if !status.success() {
        return Err("cargo build --target wasm32-unknown-unknown -p motion-web failed".to_string());
    }

    let wasm_path = root
        .join("target")
        .join("wasm32-unknown-unknown")
        .join(profile_dir)
        .join("motion_web.wasm");
    let dist_dir = root.join("target").join("web").join("dist");
    std::fs::create_dir_all(&dist_dir).map_err(|e| format!("Failed to create dist dir: {e}"))?;

    println!("==> Generating JS & WebAssembly bindings with wasm-bindgen...");
    let mut bindgen = wasm_bindgen_cli_support::Bindgen::new();
    bindgen
        .input_path(&wasm_path)
        .web(true)
        .map_err(|e| format!("wasm-bindgen web mode config error: {e}"))?
        .typescript(false)
        .generate(&dist_dir)
        .map_err(|e| format!("wasm-bindgen generation error: {e}"))?;

    let index_src = root.join("apps").join("motion-web").join("web").join("index.html");
    let index_dst = dist_dir.join("index.html");
    std::fs::copy(&index_src, &index_dst)
        .map_err(|e| format!("Failed to copy index.html: {e}"))?;

    let wasm_bg = dist_dir.join("motion_web_bg.wasm");
    let wasm_size_mb = std::fs::metadata(&wasm_bg)
        .map(|m| m.len() as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);
    println!(
        "==> Web bundle ready at {} ({wasm_size_mb:.1} MiB wasm)",
        dist_dir.display()
    );
    Ok(dist_dir)
}

fn serve_dir(dist_dir: &Path, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("Failed to bind 127.0.0.1:{port}: {e}"))?;
    println!("================================================================");
    println!("  MotionCraft Studio Web App running at: http://127.0.0.1:{port}/");
    println!("================================================================");

    for stream in listener.incoming().flatten() {
        let dir = dist_dir.to_path_buf();
        std::thread::spawn(move || {
            let mut stream = stream;
            let Ok(cloned) = stream.try_clone() else {
                return;
            };
            let mut reader = BufReader::new(cloned);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                return;
            }
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).map(|n| n == 0).unwrap_or(true)
                    || header.trim().is_empty()
                {
                    break;
                }
            }

            let raw_path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .split(['?', '#'])
                .next()
                .unwrap_or("/");
            let rel = if raw_path == "/" {
                "index.html".to_string()
            } else {
                raw_path.trim_start_matches('/').replace("..", "")
            };
            let file_path = dir.join(&rel);
            let (status, body) = match std::fs::read(&file_path) {
                Ok(bytes) => ("200 OK", bytes),
                Err(_) => ("404 Not Found", b"404 Not Found".to_vec()),
            };
            let mime = match file_path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "js" => "text/javascript; charset=utf-8",
                "wasm" => "application/wasm",
                "svg" => "image/svg+xml",
                "json" | "motion" => "application/json",
                "png" => "image/png",
                "css" => "text/css; charset=utf-8",
                _ => "application/octet-stream",
            };
            let response_headers = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: {mime}\r\n\
                 Content-Length: {}\r\n\
                 Cross-Origin-Opener-Policy: same-origin\r\n\
                 Cross-Origin-Embedder-Policy: require-corp\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream
                .write_all(response_headers.as_bytes())
                .and_then(|()| stream.write_all(&body));
        });
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let build_only = args.iter().any(|a| a == "--build-only");
    let serve_only = args.iter().any(|a| a == "--serve-only");
    let release = !args.iter().any(|a| a == "--dev");
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);

    let root = workspace_root();
    let dist_dir = if serve_only {
        root.join("target").join("web").join("dist")
    } else {
        match build_web_bundle(&root, release) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    if build_only {
        return ExitCode::SUCCESS;
    }

    if let Err(e) = serve_dir(&dist_dir, port) {
        eprintln!("Server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
