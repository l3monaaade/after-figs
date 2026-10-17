//! Bundler and HTTP server for the MotionCraft Studio Web Application (`motion-web`).
//!
//! Usage:
//! - `cargo run -p motion-web-server` — builds `motion-web` (`wasm32-unknown-unknown`),
//!   runs `wasm-bindgen` into `target/web/dist`, copies `index.html`, and serves on `http://127.0.0.1:8080`.
//! - `cargo run -p motion-web-server -- --build-only` — builds `target/web/dist` and exits.
//! - `cargo run -p motion-web-server -- --serve-only` — serves `target/web/dist` without rebuilding.
//!
//! The wasm rustc invocation remaps host paths (`--remap-path-prefix`) so local checkout
//! paths are not embedded in the binary. See README.md.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::ffi::OsString;
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

fn build_web_bundle(root: &Path, release: bool) -> Result<PathBuf, String> {
    let profile = if release { "release" } else { "dev" };
    let profile_dir = if release { "release" } else { "debug" };

    println!("==> Building motion-web (wasm32-unknown-unknown, profile={profile})...");
    let mut command = Command::new("cargo");
    command.current_dir(root).args([
        "build",
        "--target",
        "wasm32-unknown-unknown",
        "-p",
        "motion-web",
        "--profile",
        profile,
    ]);
    apply_path_remap(&mut command, root);

    let status = command
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
    std::fs::copy(&index_src, &index_dst).map_err(|e| format!("Failed to copy index.html: {e}"))?;

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

/// Rustc flags that rewrite host paths out of compiled artifacts.
///
/// Longer prefixes come first: rustc applies the first matching prefix.
/// `trim-paths` is not stable on the repo's Rust 1.95 toolchain.
fn remap_rustc_flags(root: &Path) -> Vec<String> {
    let mut prefixes: Vec<(String, &'static str)> = Vec::new();
    push_prefix(&mut prefixes, root, "/src");
    push_prefix(
        &mut prefixes,
        &root.join("vendor").join("effectcraft"),
        "/effectcraft",
    );
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        push_prefix(&mut prefixes, Path::new(&cargo_home), "/cargo");
    } else if let Some(home) = home_dir() {
        push_prefix(&mut prefixes, &home.join(".cargo"), "/cargo");
    }
    if let Some(home) = home_dir() {
        push_prefix(&mut prefixes, &home, "/home");
    }
    // Prefix baked into the previously committed `docs/motion_web_bg.wasm`.
    prefixes.push(("/Users/d3dfly-laptop".to_string(), "/user"));

    prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    prefixes.dedup_by(|a, b| a.0 == b.0);

    prefixes
        .into_iter()
        .filter(|(from, _)| from.len() > 1 && from != "/")
        .map(|(from, to)| format!("--remap-path-prefix={from}={to}"))
        .collect()
}

fn push_prefix(prefixes: &mut Vec<(String, &'static str)>, path: &Path, to: &'static str) {
    let raw = path.display().to_string();
    if let Ok(canon) = path.canonicalize() {
        let text = canon.display().to_string();
        if raw != text && raw.len() > 1 {
            prefixes.push((raw, to));
        }
        if text.len() > 1 {
            prefixes.push((text, to));
        }
    } else if raw.len() > 1 {
        prefixes.push((raw, to));
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.as_os_str().len() > 1)
}

fn apply_path_remap(cmd: &mut Command, root: &Path) {
    let mut flags = inherited_rustflags();
    println!("==> Remapping host paths out of the wasm build:");
    for flag in remap_rustc_flags(root) {
        if let Some(mapping) = flag.strip_prefix("--remap-path-prefix=") {
            println!("    {mapping}");
        }
        if !flags.iter().any(|existing| existing == &flag) {
            flags.push(flag);
        }
    }
    let encoded = flags.join("\u{1f}");
    cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded);
}

fn inherited_rustflags() -> Vec<String> {
    if let Ok(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        && !encoded.is_empty()
    {
        return encoded
            .split('\u{1f}')
            .filter(|flag| !flag.is_empty())
            .map(str::to_string)
            .collect();
    }
    match std::env::var("RUSTFLAGS") {
        Ok(flags) if !flags.trim().is_empty() => split_rustflags(&flags),
        _ => Vec::new(),
    }
}

fn split_rustflags(input: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    flags.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        flags.push(current);
    }
    flags
}

#[derive(Debug, PartialEq, Eq)]
enum StaticPathError {
    Invalid,
    EscapesRoot,
}

/// Map a request-target onto a file under `root`.
///
/// Rejects `..`, absolute paths, prefixes, and symlinks that resolve outside
/// `root`. Percent-decoding happens before the component check, so `%2e%2e` is
/// not a filename. A missing file still returns a path inside `root` (the
/// server turns the failed read into 404).
fn resolve_request_path(root: &Path, raw_path: &str) -> Result<PathBuf, StaticPathError> {
    let request = raw_path.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(request).map_err(|_| StaticPathError::Invalid)?;
    if decoded.contains('\0') {
        return Err(StaticPathError::Invalid);
    }
    let rel = decoded.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    let mut parts: Vec<OsString> = Vec::new();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(seg) => {
                if seg.is_empty() || seg == "." || seg == ".." {
                    return Err(StaticPathError::EscapesRoot);
                }
                parts.push(seg.to_os_string());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(StaticPathError::EscapesRoot);
            }
        }
    }
    if parts.is_empty() {
        parts.push(OsString::from("index.html"));
    }

    let root_canon = root.canonicalize().map_err(|_| StaticPathError::Invalid)?;
    let mut current = root_canon.clone();
    for (index, seg) in parts.iter().enumerate() {
        let next = current.join(seg);
        let last = index + 1 == parts.len();
        match std::fs::symlink_metadata(&next) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    let resolved =
                        std::fs::canonicalize(&next).map_err(|_| StaticPathError::EscapesRoot)?;
                    if !resolved.starts_with(&root_canon) {
                        return Err(StaticPathError::EscapesRoot);
                    }
                    if !last && !resolved.is_dir() {
                        return Err(StaticPathError::Invalid);
                    }
                    current = resolved;
                } else if !last && !meta.is_dir() {
                    return Err(StaticPathError::Invalid);
                } else {
                    current = next;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !current.starts_with(&root_canon) {
                    return Err(StaticPathError::EscapesRoot);
                }
                for rest in &parts[index..] {
                    current.push(rest);
                }
                return Ok(current);
            }
            Err(_) => return Err(StaticPathError::Invalid),
        }
    }
    if current.starts_with(&root_canon) {
        Ok(current)
    } else {
        Err(StaticPathError::EscapesRoot)
    }
}

fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let hi = from_hex(bytes[index + 1])?;
            let lo = from_hex(bytes[index + 2])?;
            out.push((hi << 4) | lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn from_hex(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "json" | "motion" => "application/json",
        "png" => "image/png",
        "css" => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    }
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
                if reader
                    .read_line(&mut header)
                    .map(|n| n == 0)
                    .unwrap_or(true)
                    || header.trim().is_empty()
                {
                    break;
                }
            }

            let raw_path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let (status, mime, body) = match resolve_request_path(&dir, raw_path) {
                Ok(file_path) => match std::fs::read(&file_path) {
                    Ok(bytes) => ("200 OK", mime_for(&file_path), bytes),
                    Err(_) => (
                        "404 Not Found",
                        "text/plain; charset=utf-8",
                        b"404 Not Found".to_vec(),
                    ),
                },
                Err(_) => (
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    b"400 Bad Request".to_vec(),
                ),
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(label: &str) -> Self {
            let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("motion-web-{label}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            assert!(std::fs::create_dir_all(&path).is_ok());
            Self { path }
        }

        fn canon(&self) -> PathBuf {
            self.path
                .canonicalize()
                .unwrap_or_else(|_| self.path.clone())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn serves_files_inside_root_and_index() {
        let tmp = TempRoot::new("ok");
        assert!(std::fs::write(tmp.path.join("index.html"), b"home").is_ok());
        assert!(std::fs::create_dir(tmp.path.join("sub")).is_ok());
        assert!(std::fs::write(tmp.path.join("sub").join("a.txt"), b"aaa").is_ok());

        let index = resolve_request_path(&tmp.path, "/");
        assert!(index.as_ref().is_ok_and(|p| p.ends_with("index.html")));
        if let Ok(path) = index {
            assert!(path.starts_with(tmp.canon()));
            assert_eq!(
                std::fs::read(&path).ok().as_deref(),
                Some(b"home".as_slice())
            );
        }

        let nested = resolve_request_path(&tmp.path, "/sub/./a.txt?cache=1#top");
        assert!(nested.is_ok());
        if let Ok(path) = nested {
            assert!(path.starts_with(tmp.canon()));
            assert_eq!(
                std::fs::read(&path).ok().as_deref(),
                Some(b"aaa".as_slice())
            );
        }

        let missing = resolve_request_path(&tmp.path, "/nope.txt");
        assert!(missing.is_ok());
        if let Ok(path) = missing {
            assert!(path.starts_with(tmp.canon()));
            assert!(std::fs::read(path).is_err());
        }
    }

    #[test]
    fn rejects_parent_segments_and_encoded_traversal() {
        let tmp = TempRoot::new("trav");
        for request in [
            "/../secret",
            "/foo/../../secret",
            "/%2e%2e/secret",
            "/%2E%2E/%2e%2e/etc/passwd",
            "/foo/%2e%2e/%2e%2e/secret",
            "/ok/%2f..%2f..%2fsecret",
            "/%2e%2e%2f%2e%2e%2fsecret",
            "/a%00b",
            "/%zz",
            "/%",
        ] {
            assert!(
                resolve_request_path(&tmp.path, request).is_err(),
                "{request}"
            );
        }

        // A leading encoded slash is still root-relative. It must not become `/etc/passwd`.
        let encoded_root = resolve_request_path(&tmp.path, "/%2fetc/passwd");
        assert!(encoded_root.is_ok());
        if let Ok(path) = encoded_root {
            assert!(path.starts_with(tmp.canon()));
            assert_ne!(path, PathBuf::from("/etc/passwd"));
        }
    }

    #[test]
    fn dotdot_string_replace_bypass_stays_inside_root() {
        let tmp = TempRoot::new("bypass");
        // `replace("..", "")` turned this into an absolute path (`////etc/passwd`).
        let resolved = resolve_request_path(&tmp.path, "/....//....//etc/passwd");
        assert!(resolved.is_ok());
        if let Ok(path) = resolved {
            let root = tmp.canon();
            assert!(path.starts_with(&root));
            assert_ne!(path, PathBuf::from("/etc/passwd"));
            assert!(
                path.components()
                    .any(|component| component.as_os_str() == "....")
            );
        }
    }

    #[test]
    fn single_decode_does_not_treat_double_encoding_as_parent() {
        let tmp = TempRoot::new("double");
        let resolved = resolve_request_path(&tmp.path, "/%252e%252e/secret");
        assert!(resolved.is_ok());
        if let Ok(path) = resolved {
            assert!(path.starts_with(tmp.canon()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        let tmp = TempRoot::new("link");
        let outside = TempRoot::new("outside");
        assert!(std::fs::write(outside.path.join("secret"), b"nope").is_ok());
        assert!(
            std::os::unix::fs::symlink(outside.path.join("secret"), tmp.path.join("leak")).is_ok()
        );
        assert!(resolve_request_path(&tmp.path, "/leak").is_err());
        assert!(std::os::unix::fs::symlink(&outside.path, tmp.path.join("dir")).is_ok());
        assert!(resolve_request_path(&tmp.path, "/dir/secret").is_err());

        assert!(std::fs::write(tmp.path.join("real.txt"), b"yes").is_ok());
        assert!(
            std::os::unix::fs::symlink(tmp.path.join("real.txt"), tmp.path.join("alias.txt"))
                .is_ok()
        );
        let alias = resolve_request_path(&tmp.path, "/alias.txt");
        assert!(alias.is_ok());
        if let Ok(path) = alias {
            assert!(path.starts_with(tmp.canon()));
            assert_eq!(std::fs::read(path).ok().as_deref(), Some(b"yes".as_slice()));
        }
    }

    #[test]
    fn remap_flags_cover_known_host_prefix_and_longest_first() {
        let flags = remap_rustc_flags(&workspace_root());
        assert!(
            flags
                .iter()
                .any(|flag| { flag == "--remap-path-prefix=/Users/d3dfly-laptop=/user" })
        );
        let root = workspace_root();
        let root_text = root
            .canonicalize()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        if !root_text.is_empty() {
            assert!(flags.iter().any(|flag| flag.contains(&root_text)));
        }
        let lengths: Vec<usize> = flags
            .iter()
            .filter_map(|flag| {
                flag.strip_prefix("--remap-path-prefix=")
                    .and_then(|rest| rest.split_once('='))
                    .map(|(from, _)| from.len())
            })
            .collect();
        let mut sorted = lengths.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(lengths, sorted);
    }

    #[test]
    fn split_rustflags_keeps_quoted_paths() {
        let flags =
            split_rustflags(r#"--cfg feature="a b" --remap-path-prefix=/tmp/my project=/src"#);
        assert_eq!(
            flags,
            vec![
                "--cfg".to_string(),
                "feature=a b".to_string(),
                "--remap-path-prefix=/tmp/my".to_string(),
                "project=/src".to_string(),
            ]
        );
        let quoted = split_rustflags("--remap-path-prefix=\"/tmp/my project\"=/src");
        assert_eq!(
            quoted,
            vec!["--remap-path-prefix=/tmp/my project=/src".to_string()]
        );
    }
}
