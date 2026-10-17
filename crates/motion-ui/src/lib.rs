//! Unified MotionCraft Studio UI combining FigCraft (SVG/vector authoring, layout grids,
//! auto-layout frames, constraints, boolean groups, inside/outside/variable-width strokes)
//! and EffectCraft (timeline, keyframes, expressions, shape modifiers, layer styles, effects).

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::path::PathBuf;
use std::sync::Arc;

use effectcraft_project::ItemId;
use effectcraft_ui_egui::EffectcraftApp;
use figcraft_doc::{Document, NodeId};
use figcraft_ui_egui::{FigApp, NoServices};
use motion_bridge::{
    BakeOptions, BooleanBakeMode, LayerGranularity, MotionStudioFile, TextBakeMode,
    bake_frame_to_comp, create_showcase_studio, export_frame_to_svg, find_linked_comp,
    import_svg_into_document, sync_document_to_project,
};

/// Active workspace mode in the unified MotionCraft Studio window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkspaceMode {
    /// Full-window FigCraft vector & layout design canvas with grids, frames, and constraints.
    #[default]
    Design,
    /// Full-window EffectCraft composition viewer, timeline, keyframes, and effect controls.
    Animate,
}

/// Summary of a top-level Frame on the active FigCraft page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameEntry {
    pub id: NodeId,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub grid_count: usize,
    pub has_auto_layout: bool,
    pub linked_comp: Option<ItemId>,
}

/// Unified desktop & web application state hosting both FigCraft and EffectCraft.
pub struct MotionStudioApp {
    pub mode: WorkspaceMode,
    pub fig_app: FigApp,
    pub ec_app: EffectcraftApp,
    pub bake_options: BakeOptions,
    pub auto_sync: bool,
    pub selected_frame: Option<NodeId>,
    pub studio_path: Option<PathBuf>,
    pub status_banner: String,
    last_synced_doc_rev: u64,
    fonts_synced: bool,
}

impl Default for MotionStudioApp {
    fn default() -> Self {
        Self::new_with_showcase()
    }
}

impl MotionStudioApp {
    /// Construct a new MotionStudioApp preloaded with the interactive showcase studio
    /// (demonstrating layout grids, auto-layout, boolean ops, inside/outside/variable-width
    /// strokes, and keyframed EffectCraft animations).
    pub fn new_with_showcase() -> Self {
        let studio = create_showcase_studio().unwrap_or_else(|_| {
            MotionStudioFile::new(
                "MotionCraft Studio",
                Document::new("Untitled"),
                effectcraft_project::Project::default(),
                BakeOptions::default(),
            )
        });
        Self::from_studio_file(studio, None)
    }

    /// Construct a MotionStudioApp from a [`MotionStudioFile`].
    pub fn from_studio_file(studio: MotionStudioFile, path: Option<PathBuf>) -> Self {
        let mut fig_session = figcraft_engine::Session::new();
        fig_session.prefs.theme = "dark".to_string();
        fig_session.set_ui_dark(true);

        let mut ec_session = effectcraft_host::session();
        ec_session.end_recovery();

        let mut fig_app = FigApp::new(fig_session, Box::new(NoServices));
        fig_app.ui.home = false;

        let mut ec_app = EffectcraftApp::new(ec_session);
        ec_app.ui.start_screen = false;
        ec_app.recovery = None;

        let mut app = Self {
            mode: WorkspaceMode::Design,
            fig_app,
            ec_app,
            bake_options: studio.bake_options.clone(),
            auto_sync: true,
            selected_frame: None,
            studio_path: path,
            status_banner:
                "Ready — Design vectors with grids & frames in FigCraft, then switch to Animate (EffectCraft)"
                    .to_string(),
            last_synced_doc_rev: 0,
            fonts_synced: false,
        };
        app.load_studio(studio);
        app
    }

    /// Load a [`MotionStudioFile`] into both the FigCraft and EffectCraft sessions.
    pub fn load_studio(&mut self, studio: MotionStudioFile) {
        self.bake_options = studio.bake_options;

        // 1. Load FigCraft document
        let ds = figcraft_engine::DocState::new(studio.design, None);
        let doc_rev = ds.revision;
        let first_frame = ds
            .doc
            .node(ds.page)
            .and_then(|p| {
                p.children
                    .iter()
                    .copied()
                    .find(|&id| ds.doc.node(id).is_some_and(|n| n.kind.is_frame_like()))
            });

        self.fig_app.session.docs = vec![ds];
        self.fig_app.session.active = 0;
        self.fig_app.ui.home = false;
        self.selected_frame = first_frame;

        // 2. Load EffectCraft project
        let first_comp = first_frame
            .and_then(|fid| find_linked_comp(&studio.animation, fid))
            .or_else(|| first_comp_id(&studio.animation));

        self.ec_app.session.project = Arc::new(studio.animation);
        self.ec_app.session.revision = self.ec_app.session.revision.wrapping_add(1);
        self.ec_app.ui.start_screen = false;
        self.ec_app.session.state.active_comp = first_comp;
        self.ec_app.session.state.open_comps = first_comp.into_iter().collect();
        self.ec_app.frames.clear();
        self.last_synced_doc_rev = doc_rev;
    }

    /// Snapshot the current FigCraft document, EffectCraft project, and bake options into a [`MotionStudioFile`].
    pub fn to_studio_file(&self) -> Option<MotionStudioFile> {
        let doc = self.fig_app.session.doc().ok()?.clone();
        let project = (*self.ec_app.session.project).clone();
        Some(MotionStudioFile::new(
            doc.name.clone(),
            doc,
            project,
            self.bake_options.clone(),
        ))
    }

    /// List all top-level frames on the active FigCraft page and their linked EffectCraft compositions.
    pub fn frames_on_active_page(&self) -> Vec<FrameEntry> {
        let Ok(ds) = self.fig_app.session.ds() else {
            return Vec::new();
        };
        let doc = &ds.doc;
        let Some(page_node) = doc.node(ds.page) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &child_id in &page_node.children {
            let Some(node) = doc.node(child_id) else {
                continue;
            };
            if node.kind.is_frame_like() {
                let linked_comp = find_linked_comp(&self.ec_app.session.project, child_id);
                out.push(FrameEntry {
                    id: child_id,
                    name: node.name.clone(),
                    width: node.width.round() as i32,
                    height: node.height.round() as i32,
                    grid_count: node.grids.len(),
                    has_auto_layout: node.layout.is_some(),
                    linked_comp,
                });
            }
        }
        out
    }

    /// Synchronize all frames in the active FigCraft document into the EffectCraft project,
    /// resolving auto-layout and constraints first while preserving existing keyframes & expressions.
    pub fn sync_all_frames(&mut self) -> Result<usize, String> {
        let Ok(ds) = self.fig_app.session.ds_mut() else {
            return Err("No active FigCraft document".to_string());
        };
        let mut doc_clone = (*ds.doc).clone();
        let mut project_clone = (*self.ec_app.session.project).clone();

        let reports = sync_document_to_project(&mut doc_clone, &mut project_clone, &self.bake_options)
            .map_err(|e| e.to_string())?;

        ds.doc = Arc::new(doc_clone);
        ds.revision = ds.revision.wrapping_add(1);
        self.last_synced_doc_rev = ds.revision;

        let total_layers: usize = reports.iter().map(|r| r.layers_baked).sum();
        let total_guides: usize = reports.iter().map(|r| r.guides_baked).sum();
        let count = reports.len();

        self.apply_updated_project(project_clone, self.selected_frame);
        self.status_banner = format!(
            "Auto-synced {count} frame(s) → {total_layers} layers & {total_guides} grid guides"
        );
        Ok(count)
    }

    /// Bake a single FigCraft frame into its corresponding EffectCraft composition and focus it.
    pub fn bake_selected_frame(&mut self, switch_to_animate: bool) -> Result<ItemId, String> {
        let frames = self.frames_on_active_page();
        let target_frame = self
            .selected_frame
            .filter(|id| frames.iter().any(|f| f.id == *id))
            .or_else(|| {
                self.fig_app.session.ds().ok().and_then(|ds| {
                    ds.selection
                        .iter()
                        .copied()
                        .find(|id| frames.iter().any(|f| f.id == *id))
                })
            })
            .or_else(|| frames.first().map(|f| f.id))
            .ok_or_else(|| "No Frame found on the active FigCraft page".to_string())?;

        self.selected_frame = Some(target_frame);

        let Ok(ds) = self.fig_app.session.ds_mut() else {
            return Err("No active FigCraft document".to_string());
        };
        let mut doc_clone = (*ds.doc).clone();
        let mut project_clone = (*self.ec_app.session.project).clone();

        let report =
            bake_frame_to_comp(&mut doc_clone, target_frame, &mut project_clone, &self.bake_options)
                .map_err(|e| e.to_string())?;

        ds.doc = Arc::new(doc_clone);
        ds.revision = ds.revision.wrapping_add(1);
        self.last_synced_doc_rev = ds.revision;

        self.apply_updated_project(project_clone, Some(target_frame));

        if switch_to_animate {
            self.mode = WorkspaceMode::Animate;
        }
        self.status_banner = format!(
            "Baked '{}' ({} layers, {} grid guides, {} animated layers preserved)",
            report.frame_name,
            report.layers_baked,
            report.guides_baked,
            report.preserved_animated_layers
        );
        Ok(report.comp_id)
    }

    fn apply_updated_project(
        &mut self,
        project: effectcraft_project::Project,
        focus_frame: Option<NodeId>,
    ) {
        let target_comp = focus_frame
            .and_then(|fid| find_linked_comp(&project, fid))
            .or(self.ec_app.session.state.active_comp)
            .or_else(|| first_comp_id(&project));

        self.ec_app.session.project = Arc::new(project);
        self.ec_app.session.revision = self.ec_app.session.revision.wrapping_add(1);
        self.ec_app.ui.start_screen = false;
        if let Some(cid) = target_comp {
            self.ec_app.session.state.active_comp = Some(cid);
            if !self.ec_app.session.state.open_comps.contains(&cid) {
                self.ec_app.session.state.open_comps.push(cid);
            }
        }
        self.ec_app.frames.clear();
    }

    /// Import an SVG string into the active FigCraft page.
    pub fn import_svg_str(&mut self, svg: &str, name: Option<&str>) -> Result<NodeId, String> {
        let Ok(ds) = self.fig_app.session.ds_mut() else {
            return Err("No active FigCraft document".to_string());
        };
        let mut doc_clone = (*ds.doc).clone();
        doc_clone.current_page = Some(ds.page);
        let new_frame_id =
            import_svg_into_document(&mut doc_clone, svg.as_bytes(), name).map_err(|e| e.to_string())?;
        ds.doc = Arc::new(doc_clone);
        ds.selection = vec![new_frame_id];
        ds.revision = ds.revision.wrapping_add(1);
        self.selected_frame = Some(new_frame_id);

        if self.auto_sync {
            let _ = self.sync_all_frames();
        }
        self.status_banner = format!("Imported SVG as Frame {:?} in FigCraft", new_frame_id);
        Ok(new_frame_id)
    }

    /// Export the selected FigCraft frame to an SVG string.
    pub fn export_selected_frame_svg(&self) -> Result<String, String> {
        let frames = self.frames_on_active_page();
        let target_frame = self
            .selected_frame
            .filter(|id| frames.iter().any(|f| f.id == *id))
            .or_else(|| frames.first().map(|f| f.id))
            .ok_or_else(|| "No Frame found on the active FigCraft page".to_string())?;
        let doc = self.fig_app.session.doc().map_err(|e| e.to_string())?;
        export_frame_to_svg(doc, target_frame).map_err(|e| e.to_string())
    }

    fn check_auto_sync(&mut self, pointer_down: bool) {
        if !self.auto_sync || pointer_down {
            return;
        }
        let Some(doc_rev) = self.fig_app.session.ds().ok().map(|ds| ds.revision) else {
            return;
        };
        if doc_rev != self.last_synced_doc_rev {
            let _ = self.sync_all_frames();
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            let path = file.path().to_path_buf();
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("dropped")
                .to_string();
            #[cfg(not(target_arch = "wasm32"))]
            let bytes: Option<Vec<u8>> = file
                .bytes()
                .ok()
                .or_else(|| std::fs::read(&path).ok());
            #[cfg(target_arch = "wasm32")]
            let bytes: Option<Vec<u8>> = None;

            let Some(bytes) = bytes else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".svg") {
                if let Ok(svg) = String::from_utf8(bytes) {
                    let stem = name.trim_end_matches(".svg").trim_end_matches(".SVG");
                    if let Err(e) = self.import_svg_str(&svg, Some(stem)) {
                        self.status_banner = format!("SVG import error: {e}");
                    }
                }
            } else if lower.ends_with(".motion") || lower.ends_with(".json") {
                if let Ok(text) = String::from_utf8(bytes) {
                    match MotionStudioFile::from_json(&text) {
                        Ok(studio) => {
                            self.load_studio(studio);
                            self.status_banner = format!("Loaded {name}");
                        }
                        Err(e) => {
                            self.status_banner = format!("Failed to load {name}: {e}");
                        }
                    }
                }
            }
        }
    }

    fn fonts_are_bound(ctx: &egui::Context) -> bool {
        ctx.fonts(|f| {
            let fams = f.families();
            fams.contains(&egui::FontFamily::Name("semibold".into()))
                && fams.contains(&egui::FontFamily::Name("medium".into()))
        })
    }

    /// Compact bottom toolbar (`egui::Panel::bottom`) for switching between Design (FigCraft)
    /// and Animate (EffectCraft), selecting frames, baking/auto-syncing, and file/SVG operations.
    fn draw_studio_bottom_bar(&mut self, ui: &mut egui::Ui) {
        let bg = egui::Color32::from_rgb(0x14, 0x16, 0x1B);
        let border = egui::Color32::from_rgb(0x2B, 0x2F, 0x38);
        let accent_fig = egui::Color32::from_rgb(0x0D, 0x99, 0xFF);
        let accent_ec = egui::Color32::from_rgb(0x9B, 0x6B, 0xFF);
        let accent_bake = egui::Color32::from_rgb(0x14, 0xAE, 0x5C);

        egui::Panel::bottom("motion_studio_footer")
            .exact_size(32.0)
            .frame(
                egui::Frame::NONE
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(10, 3))
                    .stroke(egui::Stroke::new(1.0, border)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;

                    // Mode switcher pill at bottom-left
                    let modes = [
                        (WorkspaceMode::Design, "✏ Design (FigCraft)", accent_fig),
                        (WorkspaceMode::Animate, "🎬 Animate (EffectCraft)", accent_ec),
                    ];
                    for (target_mode, label, active_color) in modes {
                        let active = self.mode == target_mode;
                        let btn = egui::Button::new(
                            egui::RichText::new(label)
                                .size(11.5)
                                .strong()
                                .color(if active {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_rgb(0xB8, 0xBF, 0xCC)
                                }),
                        )
                        .fill(if active {
                            active_color
                        } else {
                            egui::Color32::from_rgb(0x22, 0x25, 0x2D)
                        })
                        .corner_radius(egui::CornerRadius::same(5));

                        if ui.add(btn).clicked() && self.mode != target_mode {
                            let was_design = self.mode == WorkspaceMode::Design;
                            self.mode = target_mode;
                            if was_design
                                && target_mode == WorkspaceMode::Animate
                                && self.auto_sync
                            {
                                let _ = self.sync_all_frames();
                            }
                        }
                    }

                    ui.separator();

                    let frames = self.frames_on_active_page();
                    let current_frame = self
                        .selected_frame
                        .and_then(|id| frames.iter().find(|f| f.id == id))
                        .or_else(|| frames.first());
                    if let Some(cf) = current_frame {
                        self.selected_frame = Some(cf.id);
                    }

                    let combo_label = match current_frame {
                        Some(f) => format!(
                            "Frame: {} ({}×{}{})",
                            f.name,
                            f.width,
                            f.height,
                            if f.grid_count > 0 {
                                format!(", {} grid", f.grid_count)
                            } else {
                                String::new()
                            }
                        ),
                        None => "No Frames".to_string(),
                    };

                    egui::ComboBox::from_id_salt("motion_frame_selector")
                        .selected_text(egui::RichText::new(combo_label).size(11.0))
                        .width(195.0)
                        .show_ui(ui, |ui| {
                            for f in &frames {
                                let mut desc = format!("{} ({}×{})", f.name, f.width, f.height);
                                if f.has_auto_layout {
                                    desc.push_str(" [Auto-Layout]");
                                }
                                if f.grid_count > 0 {
                                    desc.push_str(&format!(" [{} Grid]", f.grid_count));
                                }
                                let is_sel = self.selected_frame == Some(f.id);
                                if ui.selectable_label(is_sel, desc).clicked() {
                                    self.selected_frame = Some(f.id);
                                    if let Ok(ds) = self.fig_app.session.ds_mut() {
                                        ds.selection = vec![f.id];
                                    }
                                    if let Some(cid) = f.linked_comp {
                                        self.ec_app.session.state.active_comp = Some(cid);
                                        if !self.ec_app.session.state.open_comps.contains(&cid) {
                                            self.ec_app.session.state.open_comps.push(cid);
                                        }
                                    }
                                }
                            }
                        });

                    let bake_btn = egui::Button::new(
                        egui::RichText::new("⚡ Bake → Animate")
                            .size(11.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .fill(accent_bake)
                    .corner_radius(egui::CornerRadius::same(5));
                    if ui
                        .add(bake_btn)
                        .on_hover_text(
                            "Resolve FigCraft auto-layout, constraints, boolean groups & strokes and switch to EffectCraft composition (preserving keyframes & expressions)",
                        )
                        .clicked()
                    {
                        if let Err(err) = self.bake_selected_frame(true) {
                            self.status_banner = format!("Bake error: {err}");
                        }
                    }

                    ui.checkbox(
                        &mut self.auto_sync,
                        egui::RichText::new("Auto-Sync")
                            .size(11.0)
                            .color(egui::Color32::from_rgb(0xCC, 0xD2, 0xDE)),
                    )
                    .on_hover_text(
                        "Automatically sync FigCraft vector/grid changes into EffectCraft when editing",
                    );

                    ui.menu_button(egui::RichText::new("⚙ Bridge").size(11.0), |ui| {
                        ui.set_min_width(245.0);
                        ui.label(egui::RichText::new("FigCraft → EffectCraft Bridge").strong());
                        ui.separator();

                        ui.checkbox(
                            &mut self.bake_options.preserve_animations,
                            "Preserve keyframes, expressions & modifiers",
                        );
                        ui.checkbox(
                            &mut self.bake_options.bake_grids_as_guides,
                            "Convert Layout Grids into Comp Guides",
                        );
                        ui.checkbox(
                            &mut self.bake_options.bake_grid_overlay_layer,
                            "Include [Layout Grid Overlay] Guide Layer",
                        );

                        ui.separator();
                        ui.label("Layer Granularity:");
                        ui.radio_value(
                            &mut self.bake_options.granularity,
                            LayerGranularity::TopLevelChildrenAsLayers,
                            LayerGranularity::TopLevelChildrenAsLayers.label(),
                        );
                        ui.radio_value(
                            &mut self.bake_options.granularity,
                            LayerGranularity::LeafNodesAsLayers,
                            LayerGranularity::LeafNodesAsLayers.label(),
                        );
                        ui.radio_value(
                            &mut self.bake_options.granularity,
                            LayerGranularity::SingleShapeLayer,
                            LayerGranularity::SingleShapeLayer.label(),
                        );

                        ui.separator();
                        ui.label("Boolean Groups (Union/Subtract/Intersect/Exclude):");
                        ui.radio_value(
                            &mut self.bake_options.boolean_mode,
                            BooleanBakeMode::LiveMergePaths,
                            BooleanBakeMode::LiveMergePaths.label(),
                        );
                        ui.radio_value(
                            &mut self.bake_options.boolean_mode,
                            BooleanBakeMode::BakedContours,
                            BooleanBakeMode::BakedContours.label(),
                        );

                        ui.separator();
                        ui.label("Typography:");
                        ui.radio_value(
                            &mut self.bake_options.text_mode,
                            TextBakeMode::EditableText,
                            TextBakeMode::EditableText.label(),
                        );
                        ui.radio_value(
                            &mut self.bake_options.text_mode,
                            TextBakeMode::VectorOutlines,
                            TextBakeMode::VectorOutlines.label(),
                        );
                    });

                    ui.menu_button(egui::RichText::new("📁 File / SVG").size(11.0), |ui| {
                        ui.set_min_width(210.0);
                        self.draw_file_menu_items(ui);
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(&self.status_banner)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(0x94, 0xA3, 0xB8)),
                        );
                    });
                });
            });
    }

    fn draw_file_menu_items(&mut self, ui: &mut egui::Ui) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if ui.button("📂 Open .motion Studio…").clicked() {
                ui.close();
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("MotionCraft Studio", &["motion", "json"])
                    .pick_file()
                {
                    match MotionStudioFile::load_from_path(&path) {
                        Ok(studio) => {
                            self.load_studio(studio);
                            self.studio_path = Some(path.clone());
                            self.status_banner = format!("Opened {}", path.display());
                        }
                        Err(e) => {
                            self.status_banner = format!("Open failed: {e}");
                        }
                    }
                }
            }
            if ui.button("💾 Save .motion Studio…").clicked() {
                ui.close();
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name("project.motion")
                    .add_filter("MotionCraft Studio", &["motion"])
                    .save_file()
                {
                    if let Some(studio) = self.to_studio_file() {
                        match studio.save_to_path(&path) {
                            Ok(()) => {
                                self.studio_path = Some(path.clone());
                                self.status_banner = format!("Saved {}", path.display());
                            }
                            Err(e) => {
                                self.status_banner = format!("Save failed: {e}");
                            }
                        }
                    }
                }
            }
            ui.separator();
            if ui.button("📥 Import SVG into FigCraft…").clicked() {
                ui.close();
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("SVG Vector", &["svg"])
                    .pick_file()
                {
                    match std::fs::read_to_string(&path) {
                        Ok(svg) => {
                            let stem = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("Imported SVG");
                            if let Err(e) = self.import_svg_str(&svg, Some(stem)) {
                                self.status_banner = format!("SVG import error: {e}");
                            }
                        }
                        Err(e) => {
                            self.status_banner = format!("Failed to read SVG: {e}");
                        }
                    }
                }
            }
            if ui.button("📤 Export Active Frame as SVG…").clicked() {
                ui.close();
                match self.export_selected_frame_svg() {
                    Ok(svg) => {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_file_name("frame.svg")
                            .add_filter("SVG Vector", &["svg"])
                            .save_file()
                        {
                            match std::fs::write(&path, svg) {
                                Ok(()) => {
                                    self.status_banner =
                                        format!("Exported Frame SVG to {}", path.display());
                                }
                                Err(e) => {
                                    self.status_banner = format!("Export failed: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        self.status_banner = format!("SVG export error: {e}");
                    }
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            if ui.button("💾 Download .motion Studio").clicked() {
                ui.close();
                if let Some(studio) = self.to_studio_file() {
                    match studio.to_json_pretty() {
                        Ok(json) => {
                            if browser_download("project.motion", json.as_bytes()) {
                                self.status_banner = "Downloaded project.motion".to_string();
                            } else {
                                self.status_banner = "Browser download failed".to_string();
                            }
                        }
                        Err(e) => {
                            self.status_banner = format!("Serialize failed: {e}");
                        }
                    }
                }
            }
            if ui.button("📤 Download Active Frame as SVG").clicked() {
                ui.close();
                match self.export_selected_frame_svg() {
                    Ok(svg) => {
                        if browser_download("frame.svg", svg.as_bytes()) {
                            self.status_banner = "Downloaded frame.svg".to_string();
                        } else {
                            self.status_banner = "Browser download failed".to_string();
                        }
                    }
                    Err(e) => {
                        self.status_banner = format!("SVG export error: {e}");
                    }
                }
            }
            ui.label(
                egui::RichText::new("Tip: Drag & drop .svg or .motion files anywhere onto the window to import")
                    .size(10.5)
                    .color(egui::Color32::from_rgb(0x94, 0xA3, 0xB8)),
            );
        }

        ui.separator();
        if ui.button("✨ Reset Showcase Demo").clicked() {
            ui.close();
            if let Ok(studio) = create_showcase_studio() {
                self.load_studio(studio);
                self.status_banner = "Reset to MotionCraft Showcase Studio".to_string();
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn browser_download(name: &str, bytes: &[u8]) -> bool {
    use wasm_bindgen::JsCast as _;
    let Some(window) = web_sys::window() else {
        return false;
    };
    let Some(document) = window.document() else {
        return false;
    };
    let Some(body) = document.body() else {
        return false;
    };
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let bag = web_sys::BlobPropertyBag::new();
    bag.set_type("application/octet-stream");
    let Ok(blob) = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &bag) else {
        return false;
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
        return false;
    };
    let Ok(el) = document.create_element("a") else {
        return false;
    };
    let Ok(a) = el.dyn_into::<web_sys::HtmlAnchorElement>() else {
        return false;
    };
    a.set_href(&url);
    a.set_download(name);
    let _ = body.append_child(&a);
    a.click();
    a.remove();
    let _ = web_sys::Url::revoke_object_url(&url);
    true
}

/// Find the first Composition `ItemId` in an EffectCraft project.
pub fn first_comp_id(project: &effectcraft_project::Project) -> Option<ItemId> {
    project
        .items
        .iter()
        .find_map(|(id, item)| item.as_comp().map(|_| *id))
}

impl eframe::App for MotionStudioApp {
    fn on_exit(&mut self) {
        eframe::App::on_exit(&mut self.ec_app);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if self.mode == WorkspaceMode::Animate {
            eframe::App::raw_input_hook(&mut self.ec_app, ctx, raw_input);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        eframe::App::logic(&mut self.ec_app, ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !Self::fonts_are_bound(&ctx) {
            if !self.fonts_synced {
                effectcraft_ui_egui::theme::install(&ctx, &self.ec_app.tokens, "en");
                self.fonts_synced = true;
            }
            ctx.request_repaint();
            return;
        }

        self.handle_dropped_files(&ctx);

        let pointer_down = ctx.input(|i| i.pointer.any_down());
        self.check_auto_sync(pointer_down);

        // Reserve the compact bottom studio bar first so both FigCraft and EffectCraft
        // dock cleanly above it without overlapping their own toolbars or timelines.
        self.draw_studio_bottom_bar(ui);

        match self.mode {
            WorkspaceMode::Design => {
                self.fig_app.ui(ui);
            }
            WorkspaceMode::Animate => {
                effectcraft_ui_egui::theme::apply_visuals(&ctx, &self.ec_app.tokens);
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        eframe::App::ui(&mut self.ec_app, ui, frame);
                    });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_app_initializes_showcase_and_syncs_frames() {
        let mut app = MotionStudioApp::new_with_showcase();
        assert_eq!(app.mode, WorkspaceMode::Design);
        let frames = app.frames_on_active_page();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].name, "Hero Motion Board");
        assert!(frames[0].grid_count >= 2);
        assert!(frames[0].linked_comp.is_some());

        // Baking the selected frame switches to Animate mode and returns the linked comp
        let comp_id = app.bake_selected_frame(true).unwrap();
        assert_eq!(Some(comp_id), frames[0].linked_comp);
        assert_eq!(app.mode, WorkspaceMode::Animate);

        // Exporting the active frame to SVG produces valid SVG markup
        let svg = app.export_selected_frame_svg().unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn studio_app_renders_multiple_egui_frames_without_font_panic() {
        let mut app = MotionStudioApp::new_with_showcase();
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();

        for mode in [WorkspaceMode::Design, WorkspaceMode::Animate] {
            app.mode = mode;
            for step in 0..2 {
                let raw_input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    time: Some(step as f64 * 0.016),
                    ..Default::default()
                };
                let mut out = ctx.run_ui(raw_input, |ui| {
                    eframe::App::ui(&mut app, ui, &mut frame);
                });
                out.textures_delta.clear();
            }
            assert!(MotionStudioApp::fonts_are_bound(&ctx));
        }
    }
}
