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
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn encoded_remap_rustflags(root: &Path) -> String {
    let mut flags: Vec<String> = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.split('\x1f').map(str::to_string).collect())
        .unwrap_or_default();

    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let cargo_home = std::env::var("CARGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")));

    if let Some(ref h) = home {
        flags.push(format!("--remap-path-prefix={}=/build", h.display()));
    }
    if let Some(ref ch) = cargo_home {
        flags.push(format!("--remap-path-prefix={}=/cargo", ch.display()));
        let reg_src = ch.join("registry").join("src");
        if let Ok(entries) = std::fs::read_dir(&reg_src) {
            for entry in entries.flatten() {
                flags.push(format!(
                    "--remap-path-prefix={}=/cargo/registry",
                    entry.path().display()
                ));
            }
        }
        let git_checkouts = ch.join("git").join("checkouts");
        if let Ok(repos) = std::fs::read_dir(&git_checkouts) {
            for repo in repos.flatten() {
                if let Ok(revs) = std::fs::read_dir(repo.path()) {
                    for rev in revs.flatten() {
                        flags.push(format!(
                            "--remap-path-prefix={}=/cargo/deps",
                            rev.path().display()
                        ));
                    }
                }
                flags.push(format!(
                    "--remap-path-prefix={}=/cargo/deps",
                    repo.path().display()
                ));
            }
        }
    }
    if let Ok(canon_root) = root.canonicalize() {
        flags.push(format!(
            "--remap-path-prefix={}=/workspace",
            canon_root.display()
        ));
    }
    flags.push(format!("--remap-path-prefix={}=/workspace", root.display()));
    flags.join("\x1f")
}

fn resolve_safe_path(base_dir: &Path, raw_path: &str) -> Option<PathBuf> {
    let trimmed = raw_path
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    if trimmed.contains('%') || trimmed.contains('\\') || trimmed.contains('\0') {
        return None;
    }
    let rel = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    let mut candidate = base_dir.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => {
                if seg.to_str().is_some_and(|s| s.starts_with('.')) {
                    return None;
                }
                candidate.push(seg);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return None;
            }
        }
    }
    let canon_base = base_dir.canonicalize().ok()?;
    let canon_file = candidate.canonicalize().ok()?;
    if !canon_file.starts_with(&canon_base) || !canon_file.is_file() {
        return None;
    }
    Some(canon_file)
}

fn build_web_bundle(root: &Path, release: bool) -> Result<PathBuf, String> {
    let profile = if release { "release" } else { "dev" };
    let profile_dir = if release { "release" } else { "debug" };

    println!("==> Building motion-web (wasm32-unknown-unknown, profile={profile})...");
    let status = Command::new("cargo")
        .current_dir(root)
        .env("CARGO_ENCODED_RUSTFLAGS", encoded_remap_rustflags(root))
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

    let index_src = root
        .join("apps")
        .join("motion-web")
        .join("web")
        .join("index.html");
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
    println!("  After Figs Web App running at: http://127.0.0.1:{port}/");
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

            let raw_path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let resolved = resolve_safe_path(&dir, raw_path);
            let (status, body, ext) = match resolved.as_deref().map(|p| (std::fs::read(p), p)) {
                Some((Ok(bytes), p)) => (
                    "200 OK",
                    bytes,
                    p.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                _ => ("404 Not Found", b"404 Not Found".to_vec(), String::new()),
            };
            let mime = match ext.as_str() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_safe_path_blocks_traversal_and_dotfiles() {
        let dir = std::env::temp_dir().join(format!("after-figs-web-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let index = dir.join("index.html");
        let secret = dir.join(".secret");
        let _ = std::fs::write(&index, b"ok");
        let _ = std::fs::write(&secret, b"hidden");

        assert!(resolve_safe_path(&dir, "/").is_some());
        assert!(resolve_safe_path(&dir, "/index.html?v=1#top").is_some());
        assert!(resolve_safe_path(&dir, "/../Cargo.toml").is_none());
        assert!(resolve_safe_path(&dir, "/....//Cargo.toml").is_none());
        assert!(resolve_safe_path(&dir, "/%2e%2e/Cargo.toml").is_none());
        assert!(resolve_safe_path(&dir, "/.secret").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encoded_remap_rustflags_includes_workspace_remap() {
        let flags = encoded_remap_rustflags(&workspace_root());
        assert!(flags.contains("--remap-path-prefix="));
        assert!(flags.contains("=/workspace"));
    }
}
