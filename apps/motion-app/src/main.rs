//! MotionCraft Studio — Unified desktop application and CLI combining
//! FigCraft (precision SVG/vector authoring, layout grids, auto-layout frames, constraints)
//! and EffectCraft (keyframe animation, expressions, shape modifiers, layer styles, effects).

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::io::Write;
use std::path::PathBuf;

use effectcraft_project::{ItemKind, LayerSource};
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;
use motion_bridge::{
    MotionStudioFile, create_showcase_studio, export_frame_to_svg, list_design_frames,
};
use motion_ui::MotionStudioApp;

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match run_cli_or_gui(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_cli_or_gui(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("demo") => {
            let out_path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("showcase.motion"));
            let studio = create_showcase_studio().map_err(|e| e.to_string())?;
            studio
                .save_to_path(&out_path)
                .map_err(|e| e.to_string())?;
            println!(
                "Created MotionCraft Studio showcase file at {}",
                out_path.display()
            );
            Ok(())
        }
        Some("info") => {
            let studio = if let Some(path) = args.get(1) {
                MotionStudioFile::load_from_path(PathBuf::from(path)).map_err(|e| e.to_string())?
            } else {
                create_showcase_studio().map_err(|e| e.to_string())?
            };
            print_studio_info(&studio);
            Ok(())
        }
        Some("export-svg") => {
            let input = args
                .get(1)
                .ok_or_else(|| "usage: motion-app export-svg <input.motion> <output.svg>".to_string())?;
            let output = args
                .get(2)
                .ok_or_else(|| "usage: motion-app export-svg <input.motion> <output.svg>".to_string())?;
            let studio =
                MotionStudioFile::load_from_path(PathBuf::from(input)).map_err(|e| e.to_string())?;
            let frames = list_design_frames(&studio.design);
            let (first_frame, frame_name, _, _) = frames
                .first()
                .ok_or_else(|| "No Frame found in document".to_string())?;
            let svg = export_frame_to_svg(&studio.design, *first_frame).map_err(|e| e.to_string())?;
            std::fs::write(output, svg).map_err(|e| format!("Failed to write SVG: {e}"))?;
            println!("Exported frame '{}' ({:?}) to {}", frame_name, first_frame, output);
            Ok(())
        }
        Some("render-frame") => {
            let input = args.get(1).ok_or_else(|| {
                "usage: motion-app render-frame <input.motion> <output.ppm> [time_sec]".to_string()
            })?;
            let output = args.get(2).ok_or_else(|| {
                "usage: motion-app render-frame <input.motion> <output.ppm> [time_sec]".to_string()
            })?;
            let time_sec: f64 = args
                .get(3)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.5);
            let studio =
                MotionStudioFile::load_from_path(PathBuf::from(input)).map_err(|e| e.to_string())?;
            let app = MotionStudioApp::from_studio_file(studio, None);
            let comp_id = app
                .ec_app
                .session
                .state
                .active_comp
                .or_else(|| motion_ui::first_comp_id(&app.ec_app.session.project))
                .ok_or_else(|| "Project has no active composition".to_string())?;
            let image = app.ec_app.session.render(
                comp_id,
                Tick::from_seconds_f64(time_sec),
                RenderOpts::default(),
            );
            write_ppm(PathBuf::from(output), &image)?;
            println!(
                "Rendered composition {:?} at t={:.2}s ({}x{}) to {}",
                comp_id, time_sec, image.width, image.height, output
            );
            Ok(())
        }
        Some("--help" | "-h" | "help") => {
            println!(
                "MotionCraft Studio — FigCraft (SVG/Grids/Frames) × EffectCraft (Timeline/Animation)"
            );
            println!();
            println!("Usage:");
            println!("  motion-app [path.motion]                          Launch the unified GUI studio");
            println!("  motion-app demo [out.motion]                      Generate showcase .motion file");
            println!("  motion-app info [input.motion]                    Inspect frames, grids & compositions");
            println!("  motion-app export-svg <in.motion> <out.svg>       Export primary FigCraft frame to SVG");
            println!("  motion-app render-frame <in.motion> <out.ppm> [t] Render baked EffectCraft frame at time t");
            Ok(())
        }
        other => {
            let initial_studio = if let Some(path_str) = other {
                let path = PathBuf::from(path_str);
                let studio =
                    MotionStudioFile::load_from_path(&path).map_err(|e| e.to_string())?;
                Some((studio, path))
            } else {
                None
            };
            launch_gui(initial_studio)
        }
    }
}

fn print_studio_info(studio: &MotionStudioFile) {
    println!("=== MotionCraft Studio Bundle (schema v{}) ===", studio.schema);
    println!("Studio Name: '{}'", studio.name);
    println!("FigCraft Document: '{}'", studio.design.name);
    for &page_id in &studio.design.pages {
        if let Some(page) = studio.design.node(page_id) {
            println!("  Page {:?}: '{}'", page_id, page.name);
            for &child_id in &page.children {
                if let Some(node) = studio.design.node(child_id)
                    && node.kind.is_frame_like()
                {
                    let auto_mode = node
                        .layout
                        .as_ref()
                        .map(|l| format!("{:?}", l.mode))
                        .unwrap_or_else(|| "None".to_string());
                    println!(
                        "    Frame {:?} '{}': {:.0}×{:.0}, {} layout grid(s), auto-layout={}, {} children",
                        child_id,
                        node.name,
                        node.width,
                        node.height,
                        node.grids.len(),
                        auto_mode,
                        node.children.len()
                    );
                }
            }
        }
    }

    println!("EffectCraft Project ({} item(s)):", studio.animation.items.len());
    for (item_id, item) in &studio.animation.items {
        if let ItemKind::Comp(comp) = &item.kind {
            println!(
                "  Comp {:?} '{}': {}×{} @ {:.2}fps, {} layer(s), {} guide(s)",
                item_id,
                item.name,
                comp.width,
                comp.height,
                comp.frame_rate.as_f64(),
                comp.layers.len(),
                comp.guides.len()
            );
            for layer in &comp.layers {
                let kind = match &layer.source {
                    LayerSource::Shape => "Shape",
                    LayerSource::Text => "Text",
                    LayerSource::Solid { .. } => "Solid",
                    LayerSource::Comp { .. } => "Precomp",
                    LayerSource::Null => "Null",
                    LayerSource::Camera => "Camera",
                    LayerSource::Light { .. } => "Light",
                    LayerSource::Footage { .. } => "Footage",
                    LayerSource::Model { .. } => "3D Model",
                    LayerSource::Primitive { .. } => "3D Primitive",
                };
                let animated_props = layer
                    .transform()
                    .map(|tr| {
                        ["position", "scale", "rotation", "opacity"]
                            .into_iter()
                            .filter(|name| {
                                tr.get(name)
                                    .is_some_and(|p| !p.keys.is_empty() || p.expr.is_some())
                            })
                            .count()
                    })
                    .unwrap_or(0);
                println!(
                    "    Layer {:?} '{}' [{}] (guide={}, animated_transform_channels={})",
                    layer.id, layer.name, kind, layer.switches.guide, animated_props
                );
            }
        }
    }
}

fn write_ppm(path: PathBuf, image: &effectcraft_render::Image) -> Result<(), String> {
    let rgba = image.to_rgba8();
    let mut out = Vec::with_capacity(32 + (image.width as usize * image.height as usize * 3));
    let header = format!("P6\n{} {}\n255\n", image.width, image.height);
    out.extend_from_slice(header.as_bytes());
    for px in rgba.chunks_exact(4) {
        out.push(px[0]);
        out.push(px[1]);
        out.push(px[2]);
    }
    std::fs::write(&path, out).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn launch_gui(initial: Option<(MotionStudioFile, PathBuf)>) -> Result<(), String> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("After Figs — FigCraft × EffectCraft")
        .with_inner_size([1680.0, 1020.0])
        .with_min_inner_size([1024.0, 680.0]);

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "After Figs — FigCraft × EffectCraft",
        native_options,
        Box::new(move |_cc| {
            let app = match initial {
                Some((studio, path)) => MotionStudioApp::from_studio_file(studio, Some(path)),
                None => MotionStudioApp::new_with_showcase(),
            };
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("Failed to launch GUI: {e}"))
}
