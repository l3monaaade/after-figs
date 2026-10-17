//! Embeds the optional craft-fonts build input (`CRAFT_FONTS_DIR`): both `fonts/manifest.txt` and the
//! opt-in Latin presentation fonts in `fonts/latin-manifest.txt` (Inter, Roboto, …). Unset or missing,
//! the build falls back to EffectCraft's bundled fonts (`EFFECTCRAFT_FONTS_DIR`, `EFFECTCRAFT_DIR`,
//! or `vendor/effectcraft/assets/fonts` next to this crate) and otherwise leaves `CRAFT_FONTS` empty
//! so the app uses system fonts. Neither fallback is required: a missing directory warns and the
//! build continues. Recipe: craft-fonts `docs/integration.md`.
//! Noto Sans CJK only with `FIGCRAFT_EMBED_CJK` set (the web build's only way to show CJK UI text).
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_DIR");
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_REQUIRED");
    println!("cargo::rerun-if-env-changed=EFFECTCRAFT_DIR");
    println!("cargo::rerun-if-env-changed=EFFECTCRAFT_FONTS_DIR");
    println!("cargo::rerun-if-env-changed=FIGCRAFT_EMBED_CJK");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let mut src = String::from("pub static CRAFT_FONTS: &[CraftFont] = &[\n");
    let mut ok = false;
    if let Some(dir) = std::env::var_os("CRAFT_FONTS_DIR").map(PathBuf::from) {
        for manifest in ["fonts/latin-manifest.txt", "fonts/manifest.txt"] {
            match craft_fonts(&dir, manifest, &out_dir) {
                Ok(entries) => {
                    ok = true;
                    src.push_str(&entries);
                }
                Err(e) if std::env::var_os("CRAFT_FONTS_REQUIRED").is_some() => {
                    println!("cargo::error=CRAFT_FONTS_DIR={}: {e}", dir.display());
                }
                Err(e) => println!("cargo::warning=craft-fonts {manifest} skipped: CRAFT_FONTS_DIR={}: {e}", dir.display()),
            }
        }
    }
    if !ok {
        match font_fallback_dir() {
            Some(dir) => {
                println!("cargo::rerun-if-changed={}", dir.display());
                let bundled = [
                    ("Inter", "Regular", "Inter-Regular.ttf"),
                    ("Inter", "Medium", "Inter-Medium.ttf"),
                    ("Inter", "SemiBold", "Inter-SemiBold.ttf"),
                    ("Inter", "Bold", "Inter-Bold.ttf"),
                    ("Inter", "Italic", "Inter-Italic.ttf"),
                    ("JetBrains Mono", "Regular", "JetBrainsMono-Regular.ttf"),
                    ("Noto Serif", "Regular", "NotoSerif-Regular.ttf"),
                    ("Bebas Neue", "Regular", "BebasNeue-Regular.ttf"),
                    ("Space Grotesk", "Regular", "SpaceGrotesk-Variable.ttf"),
                    ("Outfit", "Regular", "Outfit-Variable.ttf"),
                    ("Plus Jakarta Sans", "Regular", "PlusJakartaSans-Variable.ttf"),
                    ("Syne", "Regular", "Syne-Variable.ttf"),
                    ("Playfair Display", "Regular", "PlayfairDisplay-Variable.ttf"),
                    ("Cinzel", "Regular", "Cinzel-Variable.ttf"),
                    ("Oswald", "Regular", "Oswald-Variable.ttf"),
                    ("Montserrat", "Regular", "Montserrat-Variable.ttf"),
                    ("Archivo Black", "Regular", "ArchivoBlack-Regular.ttf"),
                    ("Righteous", "Regular", "Righteous-Regular.ttf"),
                    ("Pacifico", "Regular", "Pacifico-Regular.ttf"),
                    ("Permanent Marker", "Regular", "PermanentMarker-Regular.ttf"),
                    ("Anton", "Regular", "Anton-Regular.ttf"),
                    ("DM Serif Display", "Regular", "DMSerifDisplay-Regular.ttf"),
                ];
                let mut embedded = 0usize;
                for (family, style, filename) in bundled {
                    let path = dir.join(filename);
                    if path.is_file() {
                        println!("cargo::rerun-if-changed={}", path.display());
                        let include = include_bytes_arg(&path, &out_dir);
                        let _ = writeln!(src, "    CraftFont {{ family: {family:?}, style: {style:?}, bytes: include_bytes!({include}) }},");
                        embedded += 1;
                    }
                }
                if embedded == 0 {
                    println!(
                        "cargo::warning=EffectCraft font dir {} has none of the expected files; CRAFT_FONTS is empty and the app uses system fonts",
                        dir.display()
                    );
                }
            }
            None => println!(
                "cargo::warning=no CRAFT_FONTS_DIR and no EffectCraft fonts directory; CRAFT_FONTS is empty and the app uses system fonts. Set CRAFT_FONTS_DIR or EFFECTCRAFT_DIR, or check out the vendor/effectcraft submodule (see README.md)."
            ),
        }
    }
    src.push_str("];\n");
    let out = out_dir.join("craft_fonts.rs");
    if let Err(e) = std::fs::write(&out, src) {
        println!("cargo::error=writing {}: {e}", out.display());
    }
}

/// Directories tried when `CRAFT_FONTS_DIR` did not embed anything.
///
/// The relative candidate matches both a sibling EffectCraft checkout
/// (`figcraft/crates/text` → `../../../effectcraft`) and this repo's submodule
/// (`vendor/figcraft/crates/text` → `vendor/effectcraft`). A missing directory
/// is skipped; fonts are optional.
fn font_fallback_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("EFFECTCRAFT_FONTS_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("EFFECTCRAFT_DIR") {
        candidates.push(PathBuf::from(dir).join("assets").join("fonts"));
    }
    if let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("../../../effectcraft/assets/fonts"));
    }
    candidates.into_iter().find(|dir| dir.is_dir())
}

/// `include_bytes!` path relative to the generated file in `OUT_DIR`.
///
/// A relative path keeps the host checkout (`/Users/...`, Cargo home) out of the
/// generated source. `include_bytes!` resolves paths from the file that contains
/// the macro, which is `$OUT_DIR/craft_fonts.rs`.
fn include_bytes_arg(font: &Path, out_dir: &Path) -> String {
    let absolute = font.canonicalize().unwrap_or_else(|_| font.to_path_buf());
    let base = out_dir.canonicalize().unwrap_or_else(|_| out_dir.to_path_buf());
    let rendered = relative_to(&base, &absolute).unwrap_or(absolute);
    let text = rendered.to_string_lossy().replace('\\', "/");
    format!("{text:?}")
}

fn relative_to(base: &Path, target: &Path) -> Option<PathBuf> {
    let base_comps: Vec<_> = base.components().collect();
    let target_comps: Vec<_> = target.components().collect();
    let mut shared = 0;
    while shared < base_comps.len() && shared < target_comps.len() && base_comps[shared] == target_comps[shared] {
        shared += 1;
    }
    if shared == 0 {
        return None;
    }
    let mut rel = PathBuf::new();
    for _ in shared..base_comps.len() {
        rel.push("..");
    }
    for comp in &target_comps[shared..] {
        rel.push(comp.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Some(rel)
}

/// Rebuild when `manifest` changes or, while it is missing, when it is added to its folder.
///
/// Cargo takes a watched path that doesn't exist as changed on every build, and then rebuilds this
/// crate and every crate that depends on it each time. So a missing manifest is never watched
/// itself: its folder is, when there is one (a craft-fonts checkout without the Latin manifest).
/// With no craft-fonts at all there is nothing to watch, so the build can't notice it arriving:
/// the warning in `main` says how to pick it up.
fn watch(manifest: &std::path::Path) {
    let watched = if manifest.exists() { Some(manifest) } else { manifest.parent().filter(|folder| folder.is_dir()) };
    if let Some(path) = watched {
        println!("cargo::rerun-if-changed={}", path.display());
    }
}

fn craft_fonts(dir: &std::path::Path, manifest_rel: &str, out_dir: &Path) -> Result<String, String> {
    let manifest = dir.join(manifest_rel);
    watch(&manifest);
    let text = std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
    let mut out = String::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split(" | ").map(str::trim).collect();
        let [family, style, file, scripts, ..] = f.as_slice() else {
            return Err(format!("malformed manifest line: {line}"));
        };
        // CJK fonts are large; the desktop app finds system CJK fonts instead unless asked.
        let cjk = scripts.contains("Hans");
        if cjk && std::env::var_os("FIGCRAFT_EMBED_CJK").is_none() {
            continue;
        }
        // Web builds have a size budget: Inter, plus Noto Sans CJK (every CJK script, for CJK UI
        // text) when asked.
        if wasm && *family != "Inter" && !cjk {
            continue;
        }
        let path = dir.join(file);
        if !path.is_file() {
            return Err(format!("{file}: not a file under {}", dir.display()));
        }
        println!("cargo::rerun-if-changed={}", path.display());
        let include = include_bytes_arg(&path, out_dir);
        let _ = writeln!(out, "    CraftFont {{ family: {family:?}, style: {style:?}, bytes: include_bytes!({include}) }},");
    }
    Ok(out)
}
