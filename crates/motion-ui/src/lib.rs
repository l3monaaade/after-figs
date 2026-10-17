//! Unified After Figs Studio UI combining FigCraft (SVG/vector authoring, layout grids,
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use effectcraft_engine::{ExportJob, ExportResult, Exporter};
use effectcraft_project::ItemId;
use effectcraft_project::render_queue::OutputFormat;
use effectcraft_ui_egui::EffectcraftApp;
use figcraft_doc::{Document, NodeId};
use figcraft_ui_egui::FigApp;
use motion_bridge::{
    BakeOptions, BooleanBakeMode, LayerGranularity, MotionStudioFile, TextBakeMode,
    bake_frame_to_comp, create_showcase_studio, export_frame_to_svg, find_linked_comp,
    import_svg_into_document, sync_document_to_project,
};
use serde_json::json;

/// In-memory virtual file table used by browser exports, saves, and imports.
static VIRTUAL_FS: LazyLock<Mutex<BTreeMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Queue of status messages posted from async browser tasks (folder permission, system font loading, file pickers).
static PENDING_STATUS: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// Queue of `(filename, bytes)` loaded asynchronously by browser file pickers or drag-and-drop.
static PENDING_FILES: LazyLock<Mutex<Vec<(String, Vec<u8>)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Name of the user-authorized export directory (when connected via File System Access API or desktop folder picker).
static CONNECTED_EXPORT_FOLDER: LazyLock<Mutex<Option<String>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(target_arch = "wasm32")]
thread_local! {
    static EXPORT_DIR_HANDLE: std::cell::RefCell<Option<wasm_bindgen::JsValue>> =
        const { std::cell::RefCell::new(None) };
}

fn set_pending_status(msg: impl Into<String>) {
    let mut lock = PENDING_STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *lock = Some(msg.into());
}

fn take_pending_status() -> Option<String> {
    PENDING_STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn connected_export_folder_name() -> Option<String> {
    CONNECTED_EXPORT_FOLDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn set_connected_export_folder_name(name: Option<String>) {
    let mut lock = CONNECTED_EXPORT_FOLDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *lock = name;
}

/// Register raw `.ttf` / `.otf` / `.ttc` font bytes into both FigCraft and EffectCraft text engines.
pub fn register_font_bytes(bytes: Vec<u8>) -> usize {
    let fig_added = figcraft_text::db()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .add_bytes(bytes.clone())
        .len();
    figcraft_text::clear_layout_cache();
    let ec_added = effectcraft_text::fonts::add_font_data(bytes).len();
    fig_added.max(ec_added)
}

/// Return the sorted list of available font families registered in the text engine.
pub fn available_font_families() -> Vec<String> {
    figcraft_text::db()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .families()
}

fn clean_file_name(path_or_name: &str) -> &str {
    path_or_name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("after-figs-export.bin")
}

#[cfg(target_arch = "wasm32")]
fn virtual_save_path(name: &str) -> String {
    let base = clean_file_name(if name.trim().is_empty() {
        "after-figs-output"
    } else {
        name
    });
    format!("/{base}")
}

/// Store a file in `VIRTUAL_FS` and deliver it to the user (via the connected export folder or browser download on `wasm32`,
/// or native filesystem on desktop).
pub fn deliver_exported_file(path_or_name: &str, bytes: Vec<u8>) -> bool {
    let fname = clean_file_name(path_or_name).to_string();
    let vpath = format!("/{fname}");
    {
        let mut fs = VIRTUAL_FS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fs.insert(path_or_name.to_string(), bytes.clone());
        fs.insert(vpath, bytes.clone());
    }

    #[cfg(target_arch = "wasm32")]
    {
        let has_dir = EXPORT_DIR_HANDLE.with(|cell| cell.borrow().is_some());
        if has_dir {
            write_to_connected_dir_or_download(fname, bytes);
            true
        } else {
            browser_download(&fname, &bytes)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let target = std::path::Path::new(path_or_name);
        if target.is_absolute()
            && target.parent().is_some_and(|p| p != std::path::Path::new("/"))
        {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(target, &bytes).is_ok() {
                return true;
            }
        }
        if let Some(folder) = connected_export_folder_name() {
            let out_path = PathBuf::from(folder).join(&fname);
            if std::fs::write(&out_path, &bytes).is_ok() {
                return true;
            }
        }
        true
    }
}

/// Platform services for FigCraft (`FigApp`), enabling Inspector exports (PNG, SVG, PDF, JPG, WebP)
/// to download in the browser or save via file dialog on desktop.
pub struct StudioFigServices;

impl figcraft_ui_egui::Services for StudioFigServices {
    #[cfg(not(target_arch = "wasm32"))]
    fn pick_open(&mut self, title: &str, exts: &[&str]) -> figcraft_ui_egui::DialogFuture {
        let path = rfd::FileDialog::new()
            .set_title(title)
            .add_filter(title, exts)
            .pick_file();
        Box::pin(std::future::ready(path))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pick_save(
        &mut self,
        title: &str,
        name: &str,
        exts: &[&str],
    ) -> figcraft_ui_egui::DialogFuture {
        let path = rfd::FileDialog::new()
            .set_title(title)
            .set_file_name(name)
            .add_filter(title, exts)
            .save_file();
        Box::pin(std::future::ready(path))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pick_folder(&mut self, title: &str) -> figcraft_ui_egui::DialogFuture {
        let path = rfd::FileDialog::new().set_title(title).pick_folder();
        Box::pin(std::future::ready(path))
    }

    fn downloads(&self) -> bool {
        cfg!(target_arch = "wasm32") || connected_export_folder_name().is_some()
    }

    fn download(&mut self, name: &str, bytes: &[u8]) -> bool {
        let ok = deliver_exported_file(name, bytes.to_vec());
        if ok {
            set_pending_status(format!("Exported {name} ({} KB)", bytes.len() / 1024));
        }
        ok
    }
}

/// Host services for EffectCraft (`EffectcraftApp`), routing file writes (`Save Frame As`,
/// `Export Lottie`, `Export SVG`, `Save Project`) to browser downloads / connected folder on `wasm32`
/// and native disk on desktop.
pub struct StudioEcServices;

impl effectcraft_engine::Services for StudioEcServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        if let Some(bytes) = VIRTUAL_FS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
        {
            return Ok(bytes);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::fs::read(path)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{path} not found in browser virtual storage"),
            ))
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let p = std::path::Path::new(path);
            if p.parent().is_some_and(|parent| parent != std::path::Path::new("/")) {
                if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(p, data).is_ok() {
                    return Ok(());
                }
            }
        }
        if deliver_exported_file(path, data.to_vec()) {
            let fname = clean_file_name(path);
            set_pending_status(format!("Exported {fname} ({} KB)", data.len() / 1024));
            Ok(())
        } else {
            Err(std::io::Error::other("Failed to deliver exported file"))
        }
    }

    fn store_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        VIRTUAL_FS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        if VIRTUAL_FS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(path)
        {
            return true;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::path::Path::new(path).exists()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }
}

/// Cross-platform EffectCraft Render Queue exporter:
/// - On `wasm32` (and in-memory paths), writes to an in-memory buffer via `sink` and immediately
///   delivers the finished video / animation / image sequence to the user's connected Export Folder
///   or browser downloads, while using the CPU compositor on `wasm32` main thread so synchronous
///   GPU readbacks never fail.
/// - On desktop, writes directly to the filesystem path (or to the in-memory sink if targeting a virtual `/file` path).
pub struct StudioExporter {
    sink: Arc<effectcraft_export::Sink>,
}

impl Default for StudioExporter {
    fn default() -> Self {
        Self {
            sink: Arc::new(|path: &str, data: Vec<u8>| {
                let fname = clean_file_name(path).to_string();
                let kb = data.len() / 1024;
                deliver_exported_file(path, data);
                set_pending_status(format!("Rendered & exported {fname} ({kb} KB)"));
            }),
        }
    }
}

impl Exporter for StudioExporter {
    fn formats(&self) -> Vec<OutputFormat> {
        effectcraft_export::available_formats()
    }

    fn export(
        &self,
        job: &ExportJob,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<ExportResult, String> {
        effectcraft_engine::render::passes::block_on(self.export_async(job, progress))
    }

    fn export_async<'a>(
        &'a self,
        job: &'a ExportJob<'a>,
        progress: &'a mut dyn FnMut(u64, u64) -> bool,
    ) -> effectcraft_engine::render_queue::LocalFuture<'a, Result<ExportResult, String>> {
        Box::pin(async move {
            // On wasm32 (or when outputting to a virtual root path like `/Comp.mp4`), always use
            // the in-memory sink so std::fs::File::create is never called in the browser.
            let use_memory_sink = cfg!(target_arch = "wasm32")
                || std::path::Path::new(job.path).parent() == Some(std::path::Path::new("/"));

            // On wasm32 main thread, synchronous GPU readback is unavailable without a worker
            // deferred device; passing None renders cleanly on the CPU compositor.
            let accel = if cfg!(target_arch = "wasm32") {
                None
            } else {
                job.accel
            };

            let j = effectcraft_export::Job {
                project: job.project,
                footage: job.footage,
                expr: job.expr,
                accel,
                comp: job.item.comp,
                settings: &job.item.settings,
                output: &job.item.output,
                path: job.path,
                sink: if use_memory_sink {
                    Some(self.sink.as_ref())
                } else {
                    None
                },
                options: effectcraft_export::JobOptions {
                    log: job.item.log,
                    label: job.label.clone(),
                    storage: job.storage,
                    overflow: job.project.render_prefs.overflow_folders.clone(),
                },
                nested_switches: job.nested_switches,
            };
            match effectcraft_export::export_async(&j, &mut |p| progress(p.done, p.total)).await {
                Ok(r) => Ok(ExportResult {
                    path: r.path,
                    frames: r.frames,
                    width: r.width,
                    height: r.height,
                    bytes: r.bytes,
                    seconds: r.seconds,
                    audio: r.audio,
                    log: r.log,
                    overflow: r.overflow,
                }),
                Err(effectcraft_export::ExportError::Cancelled) => {
                    Err(effectcraft_engine::render_queue::CANCELLED.into())
                }
                Err(e) => Err(e.to_string()),
            }
        })
    }
}

/// Active workspace mode in the unified After Figs window.
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
                "After Figs",
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
        ec_session.services = Arc::new(StudioEcServices);
        ec_session.exporter = Some(Arc::new(StudioExporter::default()));
        ec_session.end_recovery();

        let mut fig_app = FigApp::new(fig_session, Box::new(StudioFigServices));
        fig_app.ui.home = false;

        let mut ec_app = EffectcraftApp::new(ec_session);
        ec_app.ui.start_screen = false;
        ec_app.recovery = None;
        install_ec_hooks(&mut ec_app);

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

    /// Render the active EffectCraft composition in the Render Queue with the specified format
    /// (`"H264"`, `"Gif"`, `"WebM"`, `"ProRes"`) and immediately deliver the output file.
    pub fn quick_render_active_comp(&mut self, format_name: &str, ext: &str) -> Result<String, String> {
        if self.auto_sync {
            let _ = self.sync_all_frames();
        }
        let cid = self
            .ec_app
            .session
            .active_comp_id()
            .or_else(|| first_comp_id(&self.ec_app.session.project))
            .ok_or_else(|| "No active composition to render".to_string())?;
        self.ec_app.session.state.active_comp = Some(cid);

        let comp_name = self
            .ec_app
            .session
            .project
            .item(cid)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| "Composition".to_string());
        let safe_stem: String = comp_name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let file_name = format!("{safe_stem}.{ext}");

        #[cfg(not(target_arch = "wasm32"))]
        let out_path = if let Some(folder) = connected_export_folder_name() {
            PathBuf::from(folder)
                .join(&file_name)
                .to_string_lossy()
                .to_string()
        } else if let Some(picked) = rfd::FileDialog::new()
            .set_file_name(&file_name)
            .add_filter(format_name, &[ext])
            .save_file()
        {
            picked.to_string_lossy().to_string()
        } else {
            return Err("Render cancelled".to_string());
        };

        #[cfg(target_arch = "wasm32")]
        let out_path = format!("/{file_name}");

        let added = self
            .ec_app
            .session
            .execute("renderQueue.add", json!({ "comp": cid.0 }))
            .map_err(|e| e.to_string())?;
        let item_id = added["id"]
            .as_u64()
            .or_else(|| self.ec_app.session.project.render_queue.last().map(|i| i.id))
            .ok_or_else(|| "Failed to add composition to Render Queue".to_string())?;

        self.ec_app
            .session
            .execute(
                "renderQueue.setOutputModule",
                json!({
                    "item": item_id,
                    "format": format_name,
                    "output": out_path,
                }),
            )
            .map_err(|e| e.to_string())?;

        self.ec_app
            .session
            .execute("renderQueue.render", json!({ "wait": true }))
            .map_err(|e| e.to_string())?;

        if let Some(rq_item) = self
            .ec_app
            .session
            .project
            .render_queue
            .iter()
            .find(|i| i.id == item_id)
        {
            if let effectcraft_project::render_queue::RenderStatus::Failed(reason) =
                &rq_item.status
            {
                return Err(format!("Render failed: {reason}"));
            }
        }

        let status = format!("Rendered & exported {file_name} ({format_name})");
        self.status_banner = status.clone();
        Ok(status)
    }

    /// Save the current frame of the active EffectCraft composition as a `.png`.
    pub fn quick_save_current_frame_png(&mut self) -> Result<String, String> {
        if self.auto_sync {
            let _ = self.sync_all_frames();
        }
        let cid = self
            .ec_app
            .session
            .active_comp_id()
            .or_else(|| first_comp_id(&self.ec_app.session.project))
            .ok_or_else(|| "No active composition".to_string())?;
        self.ec_app.session.state.active_comp = Some(cid);

        let comp_name = self
            .ec_app
            .session
            .project
            .item(cid)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| "Frame".to_string());
        let safe_stem: String = comp_name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let file_name = format!("{safe_stem}_frame.png");

        #[cfg(not(target_arch = "wasm32"))]
        let out_path = if let Some(folder) = connected_export_folder_name() {
            PathBuf::from(folder)
                .join(&file_name)
                .to_string_lossy()
                .to_string()
        } else if let Some(picked) = rfd::FileDialog::new()
            .set_file_name(&file_name)
            .add_filter("PNG Image", &["png"])
            .save_file()
        {
            picked.to_string_lossy().to_string()
        } else {
            return Err("Export cancelled".to_string());
        };

        #[cfg(target_arch = "wasm32")]
        let out_path = format!("/{file_name}");

        self.ec_app
            .session
            .execute(
                "comp.saveFrameAs",
                json!({ "comp": cid.0, "path": out_path }),
            )
            .map_err(|e| e.to_string())?;

        let msg = format!("Exported frame {file_name}");
        self.status_banner = msg.clone();
        Ok(msg)
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

    fn process_incoming_file_bytes(&mut self, name: &str, bytes: Vec<u8>) {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc") {
            let count = register_font_bytes(bytes);
            self.status_banner = format!(
                "Loaded font '{name}' ({count} face(s) added — {} total font families)",
                available_font_families().len()
            );
        } else if lower.ends_with(".svg") {
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

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let pending = {
            let mut q = PENDING_FILES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *q)
        };
        for (name, bytes) in pending {
            self.process_incoming_file_bytes(&name, bytes);
        }

        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let path = file.path().to_path_buf();
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("dropped")
                    .to_string();
                let bytes: Option<Vec<u8>> = file
                    .bytes()
                    .ok()
                    .or_else(|| std::fs::read(&path).ok());
                if let Some(bytes) = bytes {
                    self.process_incoming_file_bytes(&name, bytes);
                }
            }

            #[cfg(target_arch = "wasm32")]
            {
                if let Some(web_file) = file.web_file().cloned() {
                    let ctx_clone = ctx.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let fname = web_file.name();
                        if let Ok(buf) =
                            wasm_bindgen_futures::JsFuture::from(web_file.array_buffer()).await
                        {
                            let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                            PENDING_FILES
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((fname, bytes));
                            ctx_clone.request_repaint();
                        }
                    });
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
    /// and Animate (EffectCraft), selecting frames, baking/auto-syncing, exporting/rendering, and loading fonts.
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

                    ui.label(
                        egui::RichText::new("◆ After Figs")
                            .strong()
                            .size(12.0)
                            .color(egui::Color32::WHITE),
                    );
                    ui.separator();

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
                        .width(185.0)
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

                    ui.menu_button(egui::RichText::new("📤 Export / Render").size(11.0), |ui| {
                        ui.set_min_width(250.0);
                        self.draw_export_menu_items(ui);
                    });

                    ui.menu_button(egui::RichText::new("🔤 Fonts").size(11.0), |ui| {
                        ui.set_min_width(250.0);
                        self.draw_fonts_menu_items(ui);
                    });

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

                    ui.menu_button(egui::RichText::new("📁 File").size(11.0), |ui| {
                        ui.set_min_width(220.0);
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

    fn draw_export_menu_items(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Export & Render Output").strong());
        if let Some(folder) = connected_export_folder_name() {
            ui.label(
                egui::RichText::new(format!("📂 Connected Folder: {folder}"))
                    .size(11.0)
                    .color(egui::Color32::from_rgb(0x4A, 0xDE, 0x80)),
            );
        } else {
            ui.label(
                egui::RichText::new("💡 Exports download automatically, or connect a folder below")
                    .size(10.5)
                    .color(egui::Color32::from_rgb(0x94, 0xA3, 0xB8)),
            );
        }

        if ui
            .button("📂 Connect Export Folder (Grant Permission)…")
            .on_hover_text(
                "Grant permission to save rendered videos & exports directly into a folder on your computer",
            )
            .clicked()
        {
            ui.close();
            request_export_directory_permission(ui.ctx());
        }

        ui.separator();
        ui.label(egui::RichText::new("Quick Render (EffectCraft Composition)").strong());

        if ui.button("🎥 Render MP4 Video (H.264)").clicked() {
            ui.close();
            if let Err(e) = self.quick_render_active_comp("H264", "mp4") {
                self.status_banner = e;
            }
        }
        if ui.button("🎞 Render Animated GIF").clicked() {
            ui.close();
            if let Err(e) = self.quick_render_active_comp("Gif", "gif") {
                self.status_banner = e;
            }
        }
        if ui.button("🌐 Render WebM Video (VP9)").clicked() {
            ui.close();
            if let Err(e) = self.quick_render_active_comp("WebM", "webm") {
                self.status_banner = e;
            }
        }
        if ui.button("📸 Save Current Comp Frame as PNG").clicked() {
            ui.close();
            if let Err(e) = self.quick_save_current_frame_png() {
                self.status_banner = e;
            }
        }

        ui.separator();
        ui.label(egui::RichText::new("Vector & Project Export").strong());

        if ui.button("🖼 Export Active FigCraft Frame as SVG").clicked() {
            ui.close();
            match self.export_selected_frame_svg() {
                Ok(svg) => {
                    if deliver_exported_file("frame.svg", svg.into_bytes()) {
                        self.status_banner = "Exported frame.svg".to_string();
                    }
                }
                Err(e) => {
                    self.status_banner = format!("SVG export error: {e}");
                }
            }
        }

        if ui.button("📋 Open EffectCraft Render Queue…").clicked() {
            ui.close();
            if self.auto_sync {
                let _ = self.sync_all_frames();
            }
            self.mode = WorkspaceMode::Animate;
            if let Some(cid) = self.ec_app.session.active_comp_id() {
                if self.ec_app.session.project.render_queue.is_empty() {
                    let _ = self
                        .ec_app
                        .session
                        .execute("renderQueue.add", json!({ "comp": cid.0 }));
                }
            }
            self.ec_app
                .show_panel(effectcraft_ui_egui::dock::PanelKind::RenderQueue);
        }
    }

    fn draw_fonts_menu_items(&mut self, ui: &mut egui::Ui) {
        let families = available_font_families();
        ui.label(
            egui::RichText::new(format!("Typography — {} Font Families Loaded", families.len()))
                .strong(),
        );
        ui.separator();

        if ui
            .button("🖥 Load System Fonts (Browser Permission)…")
            .on_hover_text(
                "Ask the browser for permission to load fonts installed on your computer into FigCraft & EffectCraft",
            )
            .clicked()
        {
            ui.close();
            request_local_fonts_permission(ui.ctx());
        }

        if ui
            .button("➕ Upload Font Files (.ttf / .otf / .ttc)…")
            .on_hover_text("Pick .ttf, .otf, or .ttc font files from your computer to use immediately")
            .clicked()
        {
            ui.close();
            pick_and_load_font_files(ui.ctx());
        }

        ui.separator();
        ui.label(
            egui::RichText::new("Available Families (Bundled + Loaded):")
                .size(11.0)
                .color(egui::Color32::from_rgb(0x94, 0xA3, 0xB8)),
        );
        egui::ScrollArea::vertical()
            .max_height(210.0)
            .show(ui, |ui| {
                for fam in &families {
                    ui.label(egui::RichText::new(format!("• {fam}")).size(11.0));
                }
            });
    }

    fn draw_file_menu_items(&mut self, ui: &mut egui::Ui) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if ui.button("📂 Open .motion Studio…").clicked() {
                ui.close();
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("After Figs Studio", &["motion", "json"])
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
                    .add_filter("After Figs Studio", &["motion"])
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
        }

        #[cfg(target_arch = "wasm32")]
        {
            if ui.button("📂 Open .motion or .svg File…").clicked() {
                ui.close();
                pick_and_load_studio_or_svg(ui.ctx());
            }
            if ui.button("💾 Save / Download .motion Studio").clicked() {
                ui.close();
                if let Some(studio) = self.to_studio_file() {
                    match studio.to_json_pretty() {
                        Ok(json) => {
                            if deliver_exported_file("project.motion", json.into_bytes()) {
                                self.status_banner = "Saved project.motion".to_string();
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
            ui.label(
                egui::RichText::new(
                    "Tip: Drag & drop .svg, .motion, or .ttf/.otf font files anywhere onto the window",
                )
                .size(10.5)
                .color(egui::Color32::from_rgb(0x94, 0xA3, 0xB8)),
            );
        }

        ui.separator();
        if ui.button("✨ Reset After Figs Demo").clicked() {
            ui.close();
            if let Ok(studio) = create_showcase_studio() {
                self.load_studio(studio);
                self.status_banner = "Reset to After Figs Showcase".to_string();
            }
        }
    }
}

fn install_ec_hooks(ec_app: &mut EffectcraftApp) {
    #[cfg(target_arch = "wasm32")]
    {
        ec_app.hooks.pick_save = Some(Box::new(|name: &str| {
            Some(virtual_save_path(if name.is_empty() {
                "Untitled Project.ecproj"
            } else {
                name
            }))
        }));
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        ec_app.hooks.pick_save = Some(Box::new(|name: &str| {
            let default_name = clean_file_name(if name.is_empty() {
                "Untitled Project.ecproj"
            } else {
                name
            });
            if let Some(folder) = connected_export_folder_name() {
                return Some(
                    PathBuf::from(folder)
                        .join(default_name)
                        .to_string_lossy()
                        .to_string(),
                );
            }
            rfd::FileDialog::new()
                .set_file_name(default_name)
                .save_file()
                .map(|p| p.to_string_lossy().to_string())
        }));
    }
}

fn request_export_directory_permission(ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = ctx;
        if let Some(folder) = rfd::FileDialog::new()
            .set_title("Select Export Folder")
            .pick_folder()
        {
            let display = folder.to_string_lossy().to_string();
            set_connected_export_folder_name(Some(display.clone()));
            set_pending_status(format!("Connected export folder: {display}"));
        }
    }

    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast as _;
        let Some(window) = web_sys::window() else {
            return;
        };
        let picker_val =
            js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("showDirectoryPicker"))
                .unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
        let Ok(picker_fn) = picker_val.dyn_into::<js_sys::Function>() else {
            set_pending_status(
                "This browser saves exports directly to your Downloads folder automatically",
            );
            ctx.request_repaint();
            return;
        };
        let opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &opts,
            &wasm_bindgen::JsValue::from_str("mode"),
            &wasm_bindgen::JsValue::from_str("readwrite"),
        );
        let Ok(promise_val) = picker_fn.call1(&window, &opts) else {
            return;
        };
        let Ok(promise) = promise_val.dyn_into::<js_sys::Promise>() else {
            return;
        };
        let ctx_clone = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(promise).await {
                Ok(dir_handle) => {
                    let folder_name = js_sys::Reflect::get(
                        &dir_handle,
                        &wasm_bindgen::JsValue::from_str("name"),
                    )
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_else(|| "Selected Folder".to_string());
                    EXPORT_DIR_HANDLE.with(|cell| {
                        *cell.borrow_mut() = Some(dir_handle);
                    });
                    set_connected_export_folder_name(Some(folder_name.clone()));
                    set_pending_status(format!(
                        "Granted permission to '{folder_name}' — exports & renders will write directly to this folder"
                    ));
                    ctx_clone.request_repaint();
                }
                Err(err) => {
                    let msg = format!("{err:?}");
                    if !msg.contains("AbortError") && !msg.contains("abort") {
                        set_pending_status(
                            "Folder permission declined — exports will download automatically via the browser",
                        );
                        ctx_clone.request_repaint();
                    }
                }
            }
        });
    }
}

#[cfg(target_arch = "wasm32")]
fn write_to_connected_dir_or_download(file_name: String, bytes: Vec<u8>) {
    use wasm_bindgen::JsCast as _;
    let handle_opt = EXPORT_DIR_HANDLE.with(|cell| cell.borrow().clone());
    let Some(dir_handle) = handle_opt else {
        let _ = browser_download(&file_name, &bytes);
        return;
    };

    wasm_bindgen_futures::spawn_local(async move {
        let ok = async {
            let get_file_fn = js_sys::Reflect::get(
                &dir_handle,
                &wasm_bindgen::JsValue::from_str("getFileHandle"),
            )
            .ok()?
            .dyn_into::<js_sys::Function>()
            .ok()?;
            let opts = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &opts,
                &wasm_bindgen::JsValue::from_str("create"),
                &wasm_bindgen::JsValue::from_bool(true),
            );
            let file_promise = get_file_fn
                .call2(
                    &dir_handle,
                    &wasm_bindgen::JsValue::from_str(&file_name),
                    &opts,
                )
                .ok()?
                .dyn_into::<js_sys::Promise>()
                .ok()?;
            let file_handle = wasm_bindgen_futures::JsFuture::from(file_promise)
                .await
                .ok()?;

            let create_writable_fn = js_sys::Reflect::get(
                &file_handle,
                &wasm_bindgen::JsValue::from_str("createWritable"),
            )
            .ok()?
            .dyn_into::<js_sys::Function>()
            .ok()?;
            let writable_promise = create_writable_fn
                .call0(&file_handle)
                .ok()?
                .dyn_into::<js_sys::Promise>()
                .ok()?;
            let writable = wasm_bindgen_futures::JsFuture::from(writable_promise)
                .await
                .ok()?;

            let write_fn =
                js_sys::Reflect::get(&writable, &wasm_bindgen::JsValue::from_str("write"))
                    .ok()?
                    .dyn_into::<js_sys::Function>()
                    .ok()?;
            let u8_array = js_sys::Uint8Array::from(bytes.as_slice());
            let write_promise = write_fn
                .call1(&writable, &u8_array)
                .ok()?
                .dyn_into::<js_sys::Promise>()
                .ok()?;
            wasm_bindgen_futures::JsFuture::from(write_promise)
                .await
                .ok()?;

            let close_fn =
                js_sys::Reflect::get(&writable, &wasm_bindgen::JsValue::from_str("close"))
                    .ok()?
                    .dyn_into::<js_sys::Function>()
                    .ok()?;
            let close_promise = close_fn
                .call0(&writable)
                .ok()?
                .dyn_into::<js_sys::Promise>()
                .ok()?;
            wasm_bindgen_futures::JsFuture::from(close_promise)
                .await
                .ok()?;
            Some(())
        }
        .await;

        if ok.is_none() {
            let _ = browser_download(&file_name, &bytes);
        }
    });
}

fn request_local_fonts_permission(ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        pick_and_load_font_files(ctx);
    }

    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast as _;
        let Some(window) = web_sys::window() else {
            return;
        };
        let query_val =
            js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("queryLocalFonts"))
                .unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
        let Ok(query_fn) = query_val.dyn_into::<js_sys::Function>() else {
            // Browser does not support Local Font Access API; open file picker instead.
            set_pending_status(
                "Opening font file picker (Local Font Access API is Chromium-only)...",
            );
            pick_and_load_font_files(ctx);
            return;
        };

        let Ok(promise_val) = query_fn.call0(&window) else {
            pick_and_load_font_files(ctx);
            return;
        };
        let Ok(promise) = promise_val.dyn_into::<js_sys::Promise>() else {
            pick_and_load_font_files(ctx);
            return;
        };

        set_pending_status("Requesting browser permission to load local system fonts...");
        let ctx_clone = ctx.clone();
        ctx_clone.request_repaint();

        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(promise).await {
                Ok(list_val) => {
                    let list = js_sys::Array::from(&list_val);
                    let total = list.length();
                    let mut loaded_faces = 0usize;
                    let mut seen_families = std::collections::BTreeSet::new();

                    for item in list.iter() {
                        let family = js_sys::Reflect::get(
                            &item,
                            &wasm_bindgen::JsValue::from_str("family"),
                        )
                        .ok()
                        .and_then(|v| v.as_string())
                        .unwrap_or_default();
                        let style = js_sys::Reflect::get(
                            &item,
                            &wasm_bindgen::JsValue::from_str("style"),
                        )
                        .ok()
                        .and_then(|v| v.as_string())
                        .unwrap_or_default();

                        // Load Regular/Bold/Medium/Italic faces across up to 120 unique system families
                        // so loading completes quickly without freezing the browser tab.
                        let is_primary_style = style.eq_ignore_ascii_case("Regular")
                            || style.eq_ignore_ascii_case("Bold")
                            || style.eq_ignore_ascii_case("Medium")
                            || style.eq_ignore_ascii_case("Roman")
                            || !seen_families.contains(&family);
                        if !is_primary_style || seen_families.len() >= 120 {
                            continue;
                        }

                        let Some(blob_fn) = js_sys::Reflect::get(
                            &item,
                            &wasm_bindgen::JsValue::from_str("blob"),
                        )
                        .ok()
                        .and_then(|v| v.dyn_into::<js_sys::Function>().ok()) else {
                            continue;
                        };
                        let Some(blob_promise) = blob_fn
                            .call0(&item)
                            .ok()
                            .and_then(|v| v.dyn_into::<js_sys::Promise>().ok())
                        else {
                            continue;
                        };
                        let Ok(blob_val) = wasm_bindgen_futures::JsFuture::from(blob_promise).await
                        else {
                            continue;
                        };
                        let Ok(blob) = blob_val.dyn_into::<web_sys::Blob>() else {
                            continue;
                        };
                        let Ok(buf_val) =
                            wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await
                        else {
                            continue;
                        };
                        let bytes = js_sys::Uint8Array::new(&buf_val).to_vec();
                        if register_font_bytes(bytes) > 0 {
                            seen_families.insert(family);
                            loaded_faces += 1;
                        }
                    }

                    set_pending_status(format!(
                        "Loaded {loaded_faces} system font faces ({} families available out of {total} detected)",
                        available_font_families().len()
                    ));
                    ctx_clone.request_repaint();
                }
                Err(_) => {
                    set_pending_status(
                        "System font permission was not granted — use 'Upload Font Files' to add .ttf/.otf fonts",
                    );
                    ctx_clone.request_repaint();
                }
            }
        });
    }
}

fn pick_and_load_font_files(ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Load Font Files (.ttf, .otf, .ttc)")
            .add_filter("Fonts", &["ttf", "otf", "ttc"])
            .pick_files()
        {
            let mut faces = 0usize;
            for path in paths {
                if let Ok(bytes) = std::fs::read(&path) {
                    faces += register_font_bytes(bytes);
                }
            }
            set_pending_status(format!(
                "Loaded {faces} font face(s) ({} total font families)",
                available_font_families().len()
            ));
            ctx.request_repaint();
        }
    }

    #[cfg(target_arch = "wasm32")]
    {
        trigger_browser_file_input(ctx, ".ttf,.otf,.ttc", true);
    }
}

#[cfg(target_arch = "wasm32")]
fn pick_and_load_studio_or_svg(ctx: &egui::Context) {
    trigger_browser_file_input(ctx, ".motion,.json,.svg,.ttf,.otf,.ttc", true);
}

#[cfg(target_arch = "wasm32")]
fn trigger_browser_file_input(ctx: &egui::Context, accept: &str, multiple: bool) {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;

    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Some(body) = document.body() else {
        return;
    };
    let Ok(el) = document.create_element("input") else {
        return;
    };
    let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() else {
        return;
    };
    input.set_type("file");
    input.set_accept(accept);
    input.set_multiple(multiple);
    let _ = input.set_attribute("style", "display:none");
    let _ = body.append_child(&input);

    let input_clone = input.clone();
    let ctx_clone = ctx.clone();
    let onchange = Closure::once_into_js(move |_ev: web_sys::Event| {
        if let Some(files) = input_clone.files() {
            let count = files.length();
            let mut file_list = Vec::new();
            for i in 0..count {
                if let Some(f) = files.item(i) {
                    file_list.push(f);
                }
            }
            let ctx_inner = ctx_clone.clone();
            wasm_bindgen_futures::spawn_local(async move {
                for f in file_list {
                    let fname = f.name();
                    if let Ok(buf) = wasm_bindgen_futures::JsFuture::from(f.array_buffer()).await {
                        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                        PENDING_FILES
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((fname, bytes));
                    }
                }
                ctx_inner.request_repaint();
            });
        }
        input_clone.remove();
    });
    input.set_onchange(Some(onchange.unchecked_ref()));
    input.click();
}

#[cfg(target_arch = "wasm32")]
fn mime_for_name(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "wav" => "audio/wav",
        "json" | "motion" | "ecproj" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(target_arch = "wasm32")]
fn browser_download(name: &str, bytes: &[u8]) -> bool {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;
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
    bag.set_type(mime_for_name(name));
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
    let _ = a.set_attribute("style", "display:none");
    let _ = body.append_child(&a);
    a.click();
    a.remove();
    // Revoke the blob URL after 60s so large video downloads have plenty of time to start.
    let revoke = Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        revoke.unchecked_ref(),
        60_000,
    );
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

        if let Some(msg) = take_pending_status() {
            self.status_banner = msg;
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
    fn studio_exporter_and_services_export_frame_and_render_queue_in_memory() {
        let mut app = MotionStudioApp::new_with_showcase();
        let cid = app.ec_app.session.active_comp_id().unwrap();

        // 1. Test Save Frame As PNG through StudioEcServices into VIRTUAL_FS
        app.ec_app
            .session
            .execute(
                "comp.saveFrameAs",
                json!({ "comp": cid.0, "path": "/test_frame.png", "scale": 0.25 }),
            )
            .unwrap();
        let saved_png = app
            .ec_app
            .session
            .services
            .read_file("/test_frame.png")
            .unwrap();
        assert!(saved_png.starts_with(b"\x89PNG"));

        // 2. Test Render Queue export through StudioExporter into VIRTUAL_FS
        let added = app
            .ec_app
            .session
            .execute("renderQueue.add", json!({ "comp": cid.0 }))
            .unwrap();
        let item_id = added["id"].as_u64().unwrap();
        app.ec_app
            .session
            .execute(
                "renderQueue.setRenderSettings",
                json!({
                    "item": item_id,
                    "resolution": "quarter",
                    "timeSpan": "custom",
                    "start": 0.0,
                    "duration": 0.1,
                }),
            )
            .unwrap();
        app.ec_app
            .session
            .execute(
                "renderQueue.setOutputModule",
                json!({
                    "item": item_id,
                    "format": "Gif",
                    "output": "/test_anim.gif",
                }),
            )
            .unwrap();
        app.ec_app
            .session
            .execute("renderQueue.render", json!({ "wait": true }))
            .unwrap();

        let rendered_gif = app
            .ec_app
            .session
            .services
            .read_file("/test_anim.gif")
            .unwrap();
        assert!(rendered_gif.starts_with(b"GIF89a"));

        // 3. Verify 18+ font families are bundled in both FigCraft and EffectCraft
        let fig_fams = available_font_families();
        let ec_fams = effectcraft_text::fonts::families();
        assert!(
            fig_fams.len() >= 18,
            "expected >= 18 FigCraft font families, got {}: {:?}",
            fig_fams.len(),
            fig_fams
        );
        assert!(
            ec_fams.len() >= 18,
            "expected >= 18 EffectCraft font families, got {}: {:?}",
            ec_fams.len(),
            ec_fams
        );
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
