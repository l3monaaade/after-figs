//! Precision Design-to-Animation Compiler (`motion-bridge`).
//!
//! Bridges FigCraft's layout & vector authoring engine (`figcraft-doc`, `figcraft-layout`,
//! `figcraft-pathops`, `figcraft-render`, `figcraft-svg`) with EffectCraft's animation and
//! compositing engine (`effectcraft-project`, `effectcraft-path`, `effectcraft-keyframe`,
//! `effectcraft-effects`, `effectcraft-engine`).
//!
//! # Key Capabilities
//! 1. **Auto-Layout & Constraint Resolution**: Resolves FigCraft auto-layout frames (Horizontal,
//!    Vertical, Grid tracks, Hug/Fill/Fixed sizing, padding, gap) and constraints before baking.
//! 2. **Layout Grids → Composition Guides & Guide Overlays**: Converts FigCraft `LayoutGrid`s
//!    (Columns, Rows, Pixel Grid) and ruler guides into EffectCraft `Comp::guides` and an optional
//!    non-rendering `[Layout Grid Overlay]` guide shape layer (`switches.guide = true`).
//! 3. **Vector Networks, Variable-Width Profiles, Brushes & Inside/Outside Strokes**:
//!    - Center-aligned uniform strokes convert to editable `ShapePath` + `Stroke`/`Gradient Stroke`
//!      so `Trim Paths`, `Dashes`, `Taper`, and `Wave` work directly.
//!    - Inside/Outside strokes, per-side rectangle strokes, variable-width `WidthProfile` strokes,
//!      `Brush` scatter/stretch strokes, and arrowheads bake via `figcraft_render::stroke_fill`
//!      into exact filled contour shape groups.
//! 4. **Boolean Groups**: Supports both live EffectCraft `Merge Paths` (`Add`, `Subtract`,
//!    `Intersect`, `Exclude Intersections`) and pre-baked boolean contours.
//! 5. **Animation-Preserving Re-Sync**: Tracks `[figcraft:node=<id>]` and `[figcraft:frame=<id>]`
//!    metadata tags so re-baking a modified FigCraft frame updates geometry, positions, fills,
//!    strokes, and guides while preserving existing keyframes, expressions, shape modifiers
//!    (`Trim Paths`, `Repeater`, `Wiggle Paths`, etc.), and user-added layers.

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
use std::sync::Arc;

use effectcraft_color::{BlendMode as EcBlendMode, Label};
use effectcraft_keyframe::{
    Gradient as EcGradient, Justify as EcJustify, Keyframe, ShapePath, TextDoc, Value,
};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{
    Comp, Guide as EcGuide, ItemId, ItemKind, Layer, LayerSource, Node as EcPropNode, Project,
    PropGroup,
};
use effectcraft_time::{FrameRate, Tick};
use figcraft_color::{
    BlendMode as FcBlendMode, Color as FcColor, Gradient as FcGradient, GradientKind, Paint,
    PaintKind,
};
use figcraft_doc::{
    AutoLayout, BoolOp, ChildLayout, Document, EffectKind, GridAlign, GridKind, LayoutGrid,
    LayoutMode, MainAlign, Node as FcNode, NodeId, NodeKind, Sizing, StrokeAlign, StrokeCap,
    StrokeJoin, TextAlignH, TextData, VectorNetwork, Vertex, WidthPreset,
};
use figcraft_geom::{Affine, Point};
use figcraft_render::FillRule;
use serde::{Deserialize, Serialize};

/// Tag prefix embedded in `Item::comment` to link an EffectCraft composition to a FigCraft frame.
pub const FRAME_TAG_PREFIX: &str = "[figcraft:frame=";

/// Tag prefix embedded in `Layer::comment` to link an EffectCraft layer to a FigCraft node.
pub const NODE_TAG_PREFIX: &str = "[figcraft:node=";

/// Tag embedded in `Layer::comment` for the generated layout grid guide overlay layer.
pub const GRID_OVERLAY_TAG: &str = "[figcraft:grid-overlay]";

/// Tag embedded in `Layer::comment` for the generated frame background layer.
pub const FRAME_BG_TAG: &str = "[figcraft:frame-bg]";

/// Errors produced by `motion-bridge`.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("document error: {0}")]
    Doc(#[from] figcraft_doc::DocError),
    #[error("project error: {0}")]
    Project(#[from] effectcraft_project::ProjectError),
    #[error("SVG error: {0}")]
    Svg(String),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("node {0} is not a frame or artboard")]
    NotAFrame(NodeId),
}

/// How FigCraft nodes inside a frame are partitioned into EffectCraft timeline layers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayerGranularity {
    /// Each top-level child of the FigCraft frame becomes its own EffectCraft layer (nested groups
    /// become nested shape groups inside that layer). Best balance for timeline organization.
    #[default]
    TopLevelChildrenAsLayers,
    /// Every visible leaf node (or boolean group) becomes its own EffectCraft layer positioned at
    /// its world center in the frame, ready for independent keyframing.
    LeafNodesAsLayers,
    /// All vector contents of the frame are compiled into a single multi-group EffectCraft shape
    /// layer.
    SingleShapeLayer,
}

impl LayerGranularity {
    pub const ALL: [LayerGranularity; 3] = [
        LayerGranularity::TopLevelChildrenAsLayers,
        LayerGranularity::LeafNodesAsLayers,
        LayerGranularity::SingleShapeLayer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayerGranularity::TopLevelChildrenAsLayers => "Top-Level Children as Layers",
            LayerGranularity::LeafNodesAsLayers => "Every Leaf Shape as Layer",
            LayerGranularity::SingleShapeLayer => "Single Unified Shape Layer",
        }
    }
}

/// How FigCraft `BooleanOp` nodes are compiled into EffectCraft shape groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BooleanBakeMode {
    /// Emit child operand shapes followed by a live EffectCraft `Merge Paths` operator so child
    /// operands can still be animated independently in EffectCraft.
    #[default]
    LiveMergePaths,
    /// Bake the exact boolean result via `figcraft_render::node_outline` into a single contour.
    BakedContours,
}

impl BooleanBakeMode {
    pub fn label(self) -> &'static str {
        match self {
            BooleanBakeMode::LiveMergePaths => "Live Merge Paths (Animatable Operands)",
            BooleanBakeMode::BakedContours => "Baked Contours (Exact Boolean Path)",
        }
    }
}

/// How FigCraft `Text` nodes are compiled into EffectCraft layers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextBakeMode {
    /// Convert top-level text nodes into native EffectCraft `LayerSource::Text` layers (supporting
    /// Range/Wiggly/Expression Text Animators and Per-character 3D).
    #[default]
    EditableText,
    /// Convert text glyphs into vector outlines (`ShapePath`) using FigCraft's text shaper so
    /// `Trim Paths` and path modifiers can animate the glyph contours directly.
    VectorOutlines,
}

impl TextBakeMode {
    pub fn label(self) -> &'static str {
        match self {
            TextBakeMode::EditableText => "Editable Text Layer (Text Animators)",
            TextBakeMode::VectorOutlines => "Vector Glyph Outlines (Trim Paths)",
        }
    }
}

/// Options controlling how a FigCraft frame is baked/compiled into an EffectCraft composition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BakeOptions {
    pub granularity: LayerGranularity,
    /// Convert FigCraft `LayoutGrid`s (Columns, Rows, Grid) and ruler guides into EffectCraft
    /// viewer guides (`Comp::guides`).
    pub bake_grids_as_guides: bool,
    /// Also generate a non-rendering `[Layout Grid Overlay]` guide layer (`switches.guide = true`)
    /// so column/row bands are visible in the Animate viewer without appearing in final renders.
    pub bake_grid_overlay_layer: bool,
    pub boolean_mode: BooleanBakeMode,
    pub text_mode: TextBakeMode,
    /// Default frame rate for newly created compositions.
    pub frame_rate: FrameRate,
    /// Default duration in seconds for newly created compositions.
    pub duration_secs: f64,
    /// Preserve existing keyframes, expressions, shape modifiers, and user-added layers when
    /// re-syncing a previously baked composition.
    pub preserve_animations: bool,
}

impl Default for BakeOptions {
    fn default() -> Self {
        Self {
            granularity: LayerGranularity::TopLevelChildrenAsLayers,
            bake_grids_as_guides: true,
            bake_grid_overlay_layer: true,
            boolean_mode: BooleanBakeMode::LiveMergePaths,
            text_mode: TextBakeMode::EditableText,
            frame_rate: FrameRate::FPS_60,
            duration_secs: 5.0,
            preserve_animations: true,
        }
    }
}

/// Summary report returned after baking a FigCraft frame or syncing a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakeReport {
    pub frame_id: NodeId,
    pub comp_id: ItemId,
    pub frame_name: String,
    pub layers_baked: usize,
    pub guides_baked: usize,
    pub preserved_animated_layers: usize,
}

/// Unified `.motion` studio project file bundling the FigCraft design document, the EffectCraft
/// animation project, and the frame ↔ composition links.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MotionStudioFile {
    pub schema: u32,
    pub name: String,
    pub design: Document,
    pub animation: Project,
    pub bake_options: BakeOptions,
    /// Mapping from FigCraft Frame `NodeId` (`u64`) to EffectCraft Comp `ItemId` (`u64`).
    pub frame_links: BTreeMap<u64, u64>,
}

impl MotionStudioFile {
    pub const CURRENT_SCHEMA: u32 = 1;

    pub fn new(
        name: impl Into<String>,
        design: Document,
        animation: Project,
        bake_options: BakeOptions,
    ) -> Self {
        let frame_links = discover_frame_links(&design, &animation);
        Self {
            schema: Self::CURRENT_SCHEMA,
            name: name.into(),
            design,
            animation,
            bake_options,
            frame_links,
        }
    }

    pub fn to_json_pretty(&self) -> Result<String, BridgeError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(text: &str) -> Result<Self, BridgeError> {
        let mut studio: Self = serde_json::from_str(text)?;
        studio.animation.fix_next_id();
        Ok(studio)
    }

    pub fn save_to_path(&self, path: impl AsRef<std::path::Path>) -> Result<(), BridgeError> {
        let json = self.to_json_pretty()?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Self, BridgeError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json(&text)
    }
}

/// Resolve all Auto-Layout frames (`Horizontal`, `Vertical`, `Grid` tracks, `Hug`/`Fill`/`Fixed`)
/// and constraints across every page in the FigCraft document.
pub fn resolve_layout(doc: &mut Document) {
    let measure = figcraft_engine::measure::TextMeasure;
    let pages = doc.pages.clone();
    for page_id in pages {
        figcraft_layout::layout_page(doc, page_id, &measure);
    }
}

/// List all top-level frames / artboards on the active page (or across all pages if none is set).
pub fn list_design_frames(doc: &Document) -> Vec<(NodeId, String, u32, u32)> {
    let mut out = Vec::new();
    let pages: Vec<NodeId> = match doc.current_page {
        Some(p) if doc.contains(p) => vec![p],
        _ => doc.pages.clone(),
    };
    for page_id in pages {
        let Some(page) = doc.node(page_id) else {
            continue;
        };
        for &child_id in &page.children {
            let Some(node) = doc.node(child_id) else {
                continue;
            };
            if node.kind.is_frame_like() && node.visible {
                let w = node.width.round().clamp(1.0, 16384.0) as u32;
                let h = node.height.round().clamp(1.0, 16384.0) as u32;
                out.push((child_id, node.name.clone(), w, h));
            }
        }
    }
    out
}

/// Discover existing `Frame NodeId -> Comp ItemId` links stored in EffectCraft item comments.
pub fn discover_frame_links(doc: &Document, project: &Project) -> BTreeMap<u64, u64> {
    let mut links = BTreeMap::new();
    for (item_id, item) in &project.items {
        if item.as_comp().is_some()
            && let Some(fid) = parse_tag_u64(&item.comment, FRAME_TAG_PREFIX)
            && doc.contains(NodeId(fid))
        {
            links.insert(fid, item_id.0);
        }
    }
    links
}

/// Find the EffectCraft `ItemId` linked to `frame_id`, if any.
pub fn find_linked_comp(project: &Project, frame_id: NodeId) -> Option<ItemId> {
    for (item_id, item) in &project.items {
        if item.as_comp().is_some()
            && parse_tag_u64(&item.comment, FRAME_TAG_PREFIX) == Some(frame_id.0)
        {
            return Some(*item_id);
        }
    }
    None
}

/// Sync all top-level frames on the current FigCraft page into EffectCraft compositions.
pub fn sync_document_to_project(
    doc: &mut Document,
    project: &mut Project,
    options: &BakeOptions,
) -> Result<Vec<BakeReport>, BridgeError> {
    resolve_layout(doc);
    let frames = list_design_frames(doc);
    let mut reports = Vec::with_capacity(frames.len());
    for (frame_id, _, _, _) in frames {
        let report = bake_frame_to_comp(doc, frame_id, project, options)?;
        reports.push(report);
    }
    Ok(reports)
}

/// Bake a single FigCraft Frame (or Component) into a new or existing linked EffectCraft
/// composition, preserving existing keyframes, expressions, and modifiers when `options.preserve_animations` is true.
pub fn bake_frame_to_comp(
    doc: &mut Document,
    frame_id: NodeId,
    project: &mut Project,
    options: &BakeOptions,
) -> Result<BakeReport, BridgeError> {
    resolve_layout(doc);

    let frame = doc.get(frame_id)?.clone();
    if !frame.kind.is_frame_like() {
        return Err(BridgeError::NotAFrame(frame_id));
    }

    let width = frame.width.round().clamp(4.0, 16384.0) as u32;
    let height = frame.height.round().clamp(4.0, 16384.0) as u32;
    let duration = Tick::from_seconds_f64(options.duration_secs.max(0.5));
    let bg_rgb = extract_frame_background(&frame);

    let existing_comp_id = find_linked_comp(project, frame_id);
    let previous_comp = existing_comp_id.and_then(|id| project.comp(id).cloned());

    let comp_id = match existing_comp_id {
        Some(id) => {
            if let Some(item) = project.item_mut(id) {
                item.name = frame.name.clone();
            }
            id
        }
        None => {
            let mut comp = Comp::new(width, height, options.frame_rate, duration);
            comp.background = bg_rgb;
            let comment = format!("{FRAME_TAG_PREFIX}{}]", frame_id.0);
            let id = project.add_item(&frame.name, Label::Lavender, None, ItemKind::Comp(Arc::new(comp)));
            if let Some(item) = project.item_mut(id) {
                item.comment = comment;
            }
            id
        }
    };

    // Build the updated composition structure.
    let mut base_comp = match &previous_comp {
        Some(prev) => {
            let mut c = prev.clone();
            c.width = width;
            c.height = height;
            c.background = bg_rgb;
            c
        }
        None => {
            let mut c = Comp::new(width, height, options.frame_rate, duration);
            c.background = bg_rgb;
            c
        }
    };

    // 1. Compile FigCraft LayoutGrids & ruler guides into EffectCraft Comp::guides.
    let mut guides_baked = 0;
    if options.bake_grids_as_guides {
        let guides = compile_frame_guides(doc, &frame);
        guides_baked = guides.len();
        base_comp.guides = guides;
    }

    // 2. Compile FigCraft children into EffectCraft layers (in After Effects top-of-stack-first order).
    let mut generated_layers: Vec<Layer> = Vec::new();

    // Optional non-rendering Layout Grid Overlay guide layer on top of the stack.
    if options.bake_grid_overlay_layer
        && let Some(grid_layer) = build_grid_overlay_layer(project, &base_comp, &frame)
    {
        generated_layers.push(grid_layer);
    }

    let ordered_children = ordered_frame_children(&frame);

    match options.granularity {
        LayerGranularity::TopLevelChildrenAsLayers => {
            // In FigCraft, the last child in `children` (unless `reverse_z`) is on top visually.
            // In EffectCraft, `layers[0]` is on top of the layer stack.
            for &child_id in ordered_children.iter().rev() {
                let Some(child) = doc.node(child_id) else {
                    continue;
                };
                if !child.visible {
                    continue;
                }
                if let Some(layer) = compile_node_as_layer(
                    doc,
                    child,
                    Affine::IDENTITY,
                    project,
                    &base_comp,
                    options,
                ) {
                    generated_layers.push(layer);
                }
            }
        }
        LayerGranularity::LeafNodesAsLayers => {
            let mut leaves = Vec::new();
            for &child_id in &ordered_children {
                collect_leaf_nodes(doc, child_id, Affine::IDENTITY, &mut leaves);
            }
            for (leaf_id, parent_xf) in leaves.into_iter().rev() {
                let Some(leaf) = doc.node(leaf_id) else {
                    continue;
                };
                if let Some(layer) =
                    compile_node_as_layer(doc, leaf, parent_xf, project, &base_comp, options)
                {
                    generated_layers.push(layer);
                }
            }
        }
        LayerGranularity::SingleShapeLayer => {
            if let Some(unified_layer) =
                compile_frame_as_single_shape_layer(doc, &frame, project, &base_comp, options)
            {
                generated_layers.push(unified_layer);
            }
        }
    }

    // If the frame has complex fills (gradients or multiple fills) or strokes, add a dedicated
    // background shape layer at the bottom of the stack.
    if frame_needs_background_layer(&frame)
        && let Some(bg_layer) = build_frame_background_layer(doc, &frame, project, &base_comp)
    {
        generated_layers.push(bg_layer);
    }

    // 3. Reconcile with previous layers to preserve keyframes, expressions, modifiers, and
    // user-added layers (cameras, adjustment layers, nulls, etc.).
    let mut preserved_animated_layers = 0;
    if options.preserve_animations
        && let Some(prev) = &previous_comp
    {
        generated_layers = reconcile_layers_preserving_animation(
            prev,
            generated_layers,
            &mut preserved_animated_layers,
        );
    }

    let layers_baked = generated_layers.len();
    base_comp.layers = generated_layers;

    if let Some(comp_mut) = project.comp_mut(comp_id) {
        *comp_mut = base_comp;
    }

    Ok(BakeReport {
        frame_id,
        comp_id,
        frame_name: frame.name,
        layers_baked,
        guides_baked,
        preserved_animated_layers,
    })
}

/// Import an SVG byte slice into the FigCraft document as a Frame on the active page, and return
/// the new Frame's `NodeId`.
pub fn import_svg_into_document(
    doc: &mut Document,
    svg_bytes: &[u8],
    frame_name: Option<&str>,
) -> Result<NodeId, BridgeError> {
    let dst_page_id = doc
        .current_page
        .or_else(|| doc.first_page())
        .unwrap_or_else(|| doc.add_page("Page 1"));

    let existing_frames = list_design_frames(doc);
    let next_x = existing_frames
        .iter()
        .filter_map(|(id, _, w, _)| {
            doc.node(*id)
                .map(|n| n.transform.translation().x + f64::from(*w) + 80.0)
        })
        .fold(0.0, f64::max);

    let name = frame_name.unwrap_or("Imported SVG");
    let frame_id = figcraft_svg::import_svg(doc, svg_bytes, dst_page_id, next_x, 0.0, name)
        .map_err(|e| BridgeError::Svg(e.to_string()))?;
    resolve_layout(doc);
    Ok(frame_id)
}

/// Export a FigCraft Frame (or slice of nodes) to an SVG string using `figcraft-svg`.
pub fn export_frame_to_svg(doc: &Document, frame_id: NodeId) -> Result<String, BridgeError> {
    let opts = figcraft_svg::SvgOptions {
        include_ids: true,
        outline_text: false,
        simplify_stroke: true,
        bounds: None,
    };
    figcraft_svg::export_svg(doc, &[frame_id], &opts)
        .ok_or_else(|| BridgeError::Svg("nothing to export".into()))
}

fn has_visible_fill(node: &FcNode) -> bool {
    node.fills.iter().any(|p| p.visible && p.opacity > 0.0)
}

// ----------------------------------------------------------------------------
// Layout Grids → EffectCraft Guides & Guide Overlay Layer
// ----------------------------------------------------------------------------

fn compile_frame_guides(doc: &Document, frame: &FcNode) -> Vec<EcGuide> {
    let mut guides = Vec::new();
    let mut push_unique = |vertical: bool, pos: f64| {
        if !pos.is_finite() {
            return;
        }
        let exists = guides
            .iter()
            .any(|g: &EcGuide| g.vertical == vertical && (g.position - pos).abs() < 0.05);
        if !exists {
            guides.push(EcGuide {
                vertical,
                position: (pos * 100.0).round() / 100.0,
                percent: false,
            });
        }
    };

    // Frame ruler guides
    for g in &frame.guides {
        push_unique(g.vertical, g.offset);
    }

    // Snap lines from active LayoutGrids (Columns, Rows, and Grid lines up to a sensible density)
    let (xs, ys) = figcraft_layout::grid_snap_lines_in(doc, frame.id, Affine::IDENTITY);
    if xs.len() <= 64 {
        for x in xs {
            push_unique(true, x);
        }
    }
    if ys.len() <= 64 {
        for y in ys {
            push_unique(false, y);
        }
    }

    guides
}

fn build_grid_overlay_layer(project: &mut Project, comp: &Comp, frame: &FcNode) -> Option<Layer> {
    let visible_grids: Vec<&LayoutGrid> = frame.grids.iter().filter(|g| g.visible).collect();
    if visible_grids.is_empty() {
        return None;
    }

    let mut layer = build::layer(
        project,
        comp,
        "[Layout Grid Overlay]",
        LayerSource::Shape,
        (comp.width, comp.height),
        None,
    );
    layer.comment = GRID_OVERLAY_TAG.to_string();
    layer.label = Label::Cyan;
    layer.switches.guide = true;
    layer.switches.locked = true;
    layer.switches.shy = true;

    let mut ids = Ids(&mut project.next_id);
    let (cx, cy) = (f64::from(comp.width) / 2.0, f64::from(comp.height) / 2.0);
    let mut groups = Vec::new();

    for (idx, grid) in visible_grids.iter().enumerate() {
        let rects = figcraft_layout::grid_lines(grid, frame.width, frame.height);
        if rects.is_empty() {
            continue;
        }
        let mut items = Vec::new();
        match grid.kind {
            GridKind::Columns | GridKind::Rows => {
                for r in rects.iter().take(64) {
                    let rw = r.width().max(1.0);
                    let rh = r.height().max(1.0);
                    let rx = (r.x0 + r.x1) * 0.5 - cx;
                    let ry = (r.y0 + r.y1) * 0.5 - cy;
                    items.push(build::shape_rect(&mut ids, [rw, rh], [rx, ry], 0.0));
                }
                let mut fill = build::shape_fill(
                    &mut ids,
                    [
                        f64::from(grid.color.r),
                        f64::from(grid.color.g),
                        f64::from(grid.color.b),
                        1.0,
                    ],
                );
                set_prop_scalar(&mut fill, "opacity", f64::from(grid.color.a * 100.0).clamp(4.0, 45.0));
                items.push(fill);
            }
            GridKind::Grid => {
                let step = grid.size.max(4.0);
                if (frame.width / step) <= 120.0 && (frame.height / step) <= 120.0 {
                    let mut x = step;
                    while x < frame.width {
                        let p = ShapePath::polygon(
                            &[[x - cx, -cy], [x - cx, frame.height - cy]],
                            false,
                        );
                        items.push(build::shape_path(&mut ids, p));
                        x += step;
                    }
                    let mut y = step;
                    while y < frame.height {
                        let p = ShapePath::polygon(
                            &[[-cx, y - cy], [frame.width - cx, y - cy]],
                            false,
                        );
                        items.push(build::shape_path(&mut ids, p));
                        y += step;
                    }
                    let mut stroke = build::shape_stroke(
                        &mut ids,
                        [
                            f64::from(grid.color.r),
                            f64::from(grid.color.g),
                            f64::from(grid.color.b),
                            1.0,
                        ],
                        1.0,
                    );
                    set_prop_scalar(
                        &mut stroke,
                        "opacity",
                        f64::from(grid.color.a * 100.0).clamp(5.0, 35.0),
                    );
                    items.push(stroke);
                }
            }
        }
        if !items.is_empty() {
            let gname = format!("Grid {}", idx + 1);
            groups.push(build::shape_group(&mut ids, &gname, items));
        }
    }

    if groups.is_empty() {
        return None;
    }

    if let Some(contents) = layer.props.sub_mut("contents") {
        for g in groups {
            contents.children.push(g.into());
        }
    }
    Some(layer)
}

// ----------------------------------------------------------------------------
// Node → Layer & Shape Group Compilation
// ----------------------------------------------------------------------------

fn ordered_frame_children(node: &FcNode) -> Vec<NodeId> {
    let mut children = node.children.clone();
    if node.layout.as_ref().is_some_and(|l| l.reverse_z) {
        children.reverse();
    }
    children
}

fn collect_leaf_nodes(
    doc: &Document,
    node_id: NodeId,
    parent_xf: Affine,
    out: &mut Vec<(NodeId, Affine)>,
) {
    let Some(node) = doc.node(node_id) else {
        return;
    };
    if !node.visible {
        return;
    }
    let is_container = matches!(
        node.kind,
        NodeKind::Frame
            | NodeKind::Group
            | NodeKind::Section { .. }
            | NodeKind::Component { .. }
            | NodeKind::Instance { .. }
    );
    if is_container && !node.children.is_empty() {
        let next_xf = parent_xf * node.transform;
        for &cid in &ordered_frame_children(node) {
            collect_leaf_nodes(doc, cid, next_xf, out);
        }
    } else {
        out.push((node_id, parent_xf));
    }
}

fn compile_node_as_layer(
    doc: &Document,
    node: &FcNode,
    parent_xf: Affine,
    project: &mut Project,
    comp: &Comp,
    options: &BakeOptions,
) -> Option<Layer> {
    let world_xf = parent_xf * node.transform;
    let decomp = decompose_affine(world_xf, node.width, node.height);

    // Top-level editable text node -> EffectCraft Text Layer
    if options.text_mode == TextBakeMode::EditableText
        && let NodeKind::Text(text_data) = &node.kind
    {
        return Some(compile_text_node_as_layer(
            node,
            text_data,
            &decomp,
            project,
            comp,
        ));
    }

    let mut layer = build::layer(
        project,
        comp,
        &node.name,
        LayerSource::Shape,
        (comp.width, comp.height),
        None,
    );
    layer.comment = format!("{NODE_TAG_PREFIX}{}]", node.id.0);
    layer.blend_mode = convert_blend_mode(node.blend);
    layer.label = label_for_node(node);

    // Set layer transform so Anchor Point = [0, 0] (the shape group's local center) and
    // Position = [decomp.center_x, decomp.center_y] in composition coordinates.
    apply_decomposed_transform_to_layer(&mut layer, &decomp, f64::from(node.opacity) * 100.0);

    // Compile the node's own geometry and children centered around `(node.width / 2, node.height / 2)`.
    let local_center = Point::new(node.width * 0.5, node.height * 0.5);
    let center_shift = Affine::translate((-local_center.x, -local_center.y));

    let mut ids = Ids(&mut project.next_id);
    let mut root_groups = Vec::new();
    compile_node_into_shape_groups(
        doc,
        node,
        center_shift,
        &mut ids,
        options,
        &mut root_groups,
    );

    if root_groups.is_empty() {
        return None;
    }

    if let Some(contents) = layer.props.sub_mut("contents") {
        for g in root_groups {
            contents.children.push(g.into());
        }
    }

    // Apply FigCraft node effects (DropShadow, InnerShadow, LayerBlur) to the EffectCraft layer.
    apply_node_effects_to_layer(node, &mut layer, &mut ids, &comp.global_light, [
        f64::from(comp.width),
        f64::from(comp.height),
    ]);

    Some(layer)
}

fn compile_frame_as_single_shape_layer(
    doc: &Document,
    frame: &FcNode,
    project: &mut Project,
    comp: &Comp,
    options: &BakeOptions,
) -> Option<Layer> {
    let mut layer = build::layer(
        project,
        comp,
        &frame.name,
        LayerSource::Shape,
        (comp.width, comp.height),
        None,
    );
    layer.comment = format!("{NODE_TAG_PREFIX}{}]", frame.id.0);
    layer.label = Label::Blue;

    let center_shift = Affine::translate((-frame.width * 0.5, -frame.height * 0.5));
    let mut ids = Ids(&mut project.next_id);
    let mut groups = Vec::new();

    for &cid in &ordered_frame_children(frame) {
        let Some(child) = doc.node(cid) else {
            continue;
        };
        if !child.visible {
            continue;
        };
        let child_xf = center_shift * child.transform;
        compile_node_into_shape_groups(doc, child, child_xf, &mut ids, options, &mut groups);
    }

    if groups.is_empty() {
        return None;
    }

    // Note: in EffectCraft shape `Contents`, items are rendered top-to-bottom or bottom-to-top?
    // Let's check: in After Effects / EffectCraft `Contents`, the first item in `Contents` is on top!
    // Wait: let's preserve the exact visual order by reversing top-level shape groups so the last
    // FigCraft child is first in `Contents` (drawn on top).
    groups.reverse();

    if let Some(contents) = layer.props.sub_mut("contents") {
        for g in groups {
            contents.children.push(g.into());
        }
    }

    Some(layer)
}

fn compile_node_into_shape_groups(
    doc: &Document,
    node: &FcNode,
    local_to_layer: Affine,
    ids: &mut Ids,
    options: &BakeOptions,
    out_groups: &mut Vec<PropGroup>,
) {
    if !node.visible {
        return;
    }

    match &node.kind {
        NodeKind::BooleanOp { op } => {
            if let Some(g) =
                compile_boolean_node(doc, node, *op, local_to_layer, ids, options)
            {
                out_groups.push(g);
            }
        }
        NodeKind::Group
        | NodeKind::Frame
        | NodeKind::Section { .. }
        | NodeKind::Component { .. }
        | NodeKind::Instance { .. } => {
            let mut items = Vec::new();

            // If this container itself has visible fills (e.g. a card Frame background), emit its
            // own background shape group at the back of the container (which means last in AE's
            // top-first shape group list).
            let mut child_groups = Vec::new();
            for &cid in &ordered_frame_children(node) {
                if let Some(child) = doc.node(cid) {
                    compile_node_into_shape_groups(
                        doc,
                        child,
                         Affine::IDENTITY,
                        ids,
                        options,
                        &mut child_groups,
                    );
                }
            }
            // Reverse so the top-most FigCraft child sits first in AE's shape group contents.
            child_groups.reverse();

            // Container strokes (drawn above children or with background)
            if node.has_visible_stroke()
                && let Some(stroke_group) =
                    compile_standalone_stroke_group(doc, node, Affine::IDENTITY, ids)
            {
                items.push(stroke_group);
            }

            items.extend(child_groups);

            if has_visible_fill(node)
                && let Some(bg_group) = compile_container_background_group(node, ids)
            {
                items.push(bg_group);
            }

            if !items.is_empty() {
                let center = Point::new(node.width * 0.5, node.height * 0.5);
                // Shift children so the group's own anchor is at `center`.
                let shift = Affine::translate((-center.x, -center.y));
                let mut shifted_items = Vec::new();
                if node.has_visible_stroke()
                    && let Some(sg) = compile_standalone_stroke_group(doc, node, shift, ids)
                {
                    shifted_items.push(sg);
                }
                let mut c_groups = Vec::new();
                for &cid in &ordered_frame_children(node) {
                    if let Some(child) = doc.node(cid) {
                        compile_node_into_shape_groups(
                            doc,
                            child,
                            shift * child.transform,
                            ids,
                            options,
                            &mut c_groups,
                        );
                    }
                }
                c_groups.reverse();
                shifted_items.extend(c_groups);
                if has_visible_fill(node)
                    && let Some(bg_group) = compile_leaf_shape_group_with_xf(doc, node, shift, ids)
                {
                    shifted_items.push(bg_group);
                }

                let mut grp = build::shape_group(ids, &node.name, shifted_items);
                apply_affine_to_shape_group_transform(
                    &mut grp,
                    local_to_layer * Affine::translate((center.x, center.y)),
                    f64::from(node.opacity) * 100.0,
                );
                out_groups.push(grp);
            }
        }
        _ => {
            if let Some(g) = compile_leaf_node_shape_group(doc, node, local_to_layer, ids) {
                out_groups.push(g);
            }
        }
    }
}

fn compile_container_background_group(node: &FcNode, ids: &mut Ids) -> Option<PropGroup> {
    let shift = Affine::translate((-node.width * 0.5, -node.height * 0.5));
    let mut items = Vec::new();
    let uniform_corner = node.corners.uniform().unwrap_or(0.0);
    if node.corners.uniform().is_some() && node.corners.smoothing == 0.0 {
        items.push(build::shape_rect(
            ids,
            [node.width, node.height],
            [0.0, 0.0],
            uniform_corner,
        ));
    } else if let Some(mut path) = figcraft_doc::geometry::shape_path(node) {
        path.apply_affine(shift);
        for sp in effectcraft_path::from_kurbo(&path) {
            items.push(build::shape_path(ids, sp));
        }
    }
    append_fill_paints(node, shift, ids, FillRule::Winding, &mut items);
    if items.is_empty() {
        None
    } else {
        Some(build::shape_group(ids, &format!("{} Background", node.name), items))
    }
}

fn compile_leaf_shape_group_with_xf(
    doc: &Document,
    node: &FcNode,
    xf: Affine,
    ids: &mut Ids,
) -> Option<PropGroup> {
    let mut items = Vec::new();
    let Some((mut outline, rule)) = figcraft_render::node_outline(doc, node) else {
        return None;
    };
    outline.apply_affine(xf);
    for sp in effectcraft_path::from_kurbo(&outline) {
        items.push(build::shape_path(ids, sp));
    }
    append_fill_paints(node, xf, ids, rule, &mut items);
    if items.is_empty() {
        None
    } else {
        Some(build::shape_group(ids, &format!("{} Fill", node.name), items))
    }
}

fn compile_leaf_node_shape_group(
    doc: &Document,
    node: &FcNode,
    local_to_layer: Affine,
    ids: &mut Ids,
) -> Option<PropGroup> {
    let center = Point::new(node.width * 0.5, node.height * 0.5);
    let center_shift = Affine::translate((-center.x, -center.y));

    let mut items: Vec<PropGroup> = Vec::new();
    let mut fill_rule = FillRule::Winding;

    // 1. Emit parametric or Bezier path geometry centered at (0, 0).
    let can_use_center_stroke = can_use_parametric_stroke(node);

    match &node.kind {
        NodeKind::Rectangle if node.corners.uniform().is_some() && node.corners.smoothing == 0.0 => {
            let r = node.corners.uniform().unwrap_or(0.0);
            items.push(build::shape_rect(ids, [node.width, node.height], [0.0, 0.0], r));
        }
        NodeKind::Ellipse { start, sweep, inner }
            if start.abs() < 1e-6
                && (sweep.abs() - std::f64::consts::TAU).abs() < 1e-4
                && *inner == 0.0 =>
        {
            items.push(build::shape_ellipse(ids, [node.width, node.height], [0.0, 0.0]));
        }
        _ => {
            // Use exact outline for fills, or stroke_geometry when stroke-only open path
            if has_visible_fill(node) || !node.has_visible_stroke() {
                if let Some((mut outline, rule)) = figcraft_render::node_outline(doc, node) {
                    fill_rule = rule;
                    outline.apply_affine(center_shift);
                    for sp in effectcraft_path::from_kurbo(&outline) {
                        items.push(build::shape_path(ids, sp));
                    }
                }
            }
            if can_use_center_stroke
                && !has_visible_fill(node)
                && let Some(mut stroke_geom) = figcraft_doc::geometry::stroke_geometry(node)
            {
                stroke_geom.apply_affine(center_shift);
                for sp in effectcraft_path::from_kurbo(&stroke_geom) {
                    items.push(build::shape_path(ids, sp));
                }
            }
        }
    }

    // 2. If the node has both a fill and a separate open/complex stroke geometry (like a VectorNetwork
    // with open segments + filled regions, or Inside/Outside/Variable-Width/Brush strokes), handle
    // the stroke faithfully:
    if node.has_visible_stroke() {
        if can_use_center_stroke {
            // If it's a VectorNetwork that had filled regions AND open stroke segments, or a normal
            // shape where the path is already in `items`:
            if matches!(node.kind, NodeKind::Vector { .. }) && has_visible_fill(node) {
                // Separate subgroup for the stroke so Trim Paths only trims the stroke geometry
                if let Some(mut stroke_geom) = figcraft_doc::geometry::stroke_geometry(node) {
                    stroke_geom.apply_affine(center_shift);
                    let mut stroke_items = Vec::new();
                    for sp in effectcraft_path::from_kurbo(&stroke_geom) {
                        stroke_items.push(build::shape_path(ids, sp));
                    }
                    append_center_stroke_paints(node, center_shift, ids, &mut stroke_items);
                    items.push(build::shape_group(ids, "Stroke Path", stroke_items));
                }
            } else {
                append_center_stroke_paints(node, center_shift, ids, &mut items);
            }
        } else if let Some(baked_stroke_group) =
            compile_standalone_stroke_group(doc, node, center_shift, ids)
        {
            items.push(baked_stroke_group);
        }
    }

    // 3. Emit Fill / Gradient Fill paints.
    if has_visible_fill(node) {
        append_fill_paints(node, center_shift, ids, fill_rule, &mut items);
    }

    if items.is_empty() {
        return None;
    }

    let mut group = build::shape_group(ids, &node.name, items);
    apply_affine_to_shape_group_transform(
        &mut group,
        local_to_layer * Affine::translate((center.x, center.y)),
        f64::from(node.opacity) * 100.0,
    );
    Some(group)
}

/// Returns `true` when a node's stroke can be represented as a native EffectCraft centered stroke
/// (supporting `Trim Paths`, `Dashes`, `Taper`, `Wave`). Returns `false` when the stroke uses
/// FigCraft-specific contour features (`Inside`/`Outside` alignment on closed paths, per-side
/// weights, `WidthProfile` variable width, `Brush` strokes, or arrow caps), in which case we bake
/// the exact contour via `figcraft_render::stroke_fill`.
fn can_use_parametric_stroke(node: &FcNode) -> bool {
    if !node.has_visible_stroke() {
        return false;
    }
    let s = &node.stroke;
    let closed = figcraft_doc::geometry::is_closed(node);
    let align_ok = s.align == StrokeAlign::Center || !closed;
    let no_sides = s.sides.is_none();
    let no_profile = s.profile().is_none();
    let no_brush = s.brush.is_none();
    let no_arrows = !s.cap_start.is_arrow() && !s.cap_end.is_arrow();
    let no_point_styles = match &node.kind {
        NodeKind::Vector { network } => !network.has_point_styles(),
        _ => true,
    };
    align_ok && no_sides && no_profile && no_brush && no_arrows && no_point_styles
}

fn compile_standalone_stroke_group(
    doc: &Document,
    node: &FcNode,
    xf: Affine,
    ids: &mut Ids,
) -> Option<PropGroup> {
    let (stroke_bez, rule) = figcraft_render::stroke_fill(doc, node)?;
    let mut path = (*stroke_bez).clone();
    path.apply_affine(xf);
    let shape_paths = effectcraft_path::from_kurbo(&path);
    if shape_paths.is_empty() {
        return None;
    }
    let mut items = Vec::with_capacity(shape_paths.len() + node.strokes.len());
    for sp in shape_paths {
        items.push(build::shape_path(ids, sp));
    }
    append_paints_as_fills(
        &node.strokes,
        node.width,
        node.height,
        xf,
        ids,
        rule,
        &mut items,
    );
    Some(build::shape_group(ids, &format!("{} Stroke Outline", node.name), items))
}

fn compile_boolean_node(
    doc: &Document,
    node: &FcNode,
    op: BoolOp,
    local_to_layer: Affine,
    ids: &mut Ids,
    options: &BakeOptions,
) -> Option<PropGroup> {
    let center = Point::new(node.width * 0.5, node.height * 0.5);
    let center_shift = Affine::translate((-center.x, -center.y));

    let uniform_child_ops = node
        .children
        .iter()
        .skip(1)
        .filter_map(|cid| doc.node(*cid))
        .filter(|c| c.visible)
        .all(|c| c.bool_op.unwrap_or(op) == op);

    let mut items = Vec::new();

    if options.boolean_mode == BooleanBakeMode::LiveMergePaths
        && uniform_child_ops
        && !can_use_baked_only_stroke(node)
    {
        for &cid in &node.children {
            let Some(child) = doc.node(cid) else {
                continue;
            };
            if !child.visible {
                continue;
            }
            let Some((mut child_path, _)) = figcraft_render::subtree_outline(doc, child, 0) else {
                continue;
            };
            child_path.apply_affine(center_shift * child.transform);
            let sub_paths = effectcraft_path::from_kurbo(&child_path);
            if sub_paths.is_empty() {
                continue;
            }
            let mut operand_items = Vec::with_capacity(sub_paths.len());
            for sp in sub_paths {
                operand_items.push(build::shape_path(ids, sp));
            }
            items.push(build::shape_group(ids, &child.name, operand_items));
        }

        if let Some(mut merge) = build::shape_simple_op(ids, "merge") {
            // EffectCraft Merge Paths popup: 0=Merge, 1=Add, 2=Subtract, 3=Intersect, 4=Exclude Intersections
            let mode_idx = match op {
                BoolOp::Union => 1,
                BoolOp::Subtract => 2,
                BoolOp::Intersect => 3,
                BoolOp::Exclude => 4,
            };
            set_prop_enum(&mut merge, "mode", mode_idx);
            items.push(merge);
        }

        if node.has_visible_stroke() {
            append_center_stroke_paints(node, center_shift, ids, &mut items);
        }
        if has_visible_fill(node) {
            append_fill_paints(node, center_shift, ids, FillRule::EvenOdd, &mut items);
        }
    } else {
        // Baked boolean contour
        let (mut outline, rule) = figcraft_render::node_outline(doc, node)?;
        outline.apply_affine(center_shift);
        for sp in effectcraft_path::from_kurbo(&outline) {
            items.push(build::shape_path(ids, sp));
        }
        if node.has_visible_stroke()
            && let Some(sg) = compile_standalone_stroke_group(doc, node, center_shift, ids)
        {
            items.push(sg);
        }
        if has_visible_fill(node) {
            append_fill_paints(node, center_shift, ids, rule, &mut items);
        }
    }

    if items.is_empty() {
        return None;
    }

    let mut group = build::shape_group(ids, &node.name, items);
    apply_affine_to_shape_group_transform(
        &mut group,
        local_to_layer * Affine::translate((center.x, center.y)),
        f64::from(node.opacity) * 100.0,
    );
    Some(group)
}

fn can_use_baked_only_stroke(node: &FcNode) -> bool {
    node.has_visible_stroke() && !can_use_parametric_stroke(node)
}

fn compile_text_node_as_layer(
    node: &FcNode,
    text_data: &TextData,
    decomp: &DecomposedTransform,
    project: &mut Project,
    comp: &Comp,
) -> Layer {
    let mut layer = build::layer(
        project,
        comp,
        &node.name,
        LayerSource::Text,
        (comp.width, comp.height),
        None,
    );
    layer.comment = format!("{NODE_TAG_PREFIX}{}]", node.id.0);
    layer.blend_mode = convert_blend_mode(node.blend);
    layer.label = Label::Red;

    let fill_rgba = node
        .fills
        .iter()
        .rev()
        .find(|p| p.visible)
        .map(Paint::preview_color)
        .unwrap_or(FcColor::WHITE);

    let justify = match text_data.align_h {
        TextAlignH::Left => EcJustify::Left,
        TextAlignH::Center => EcJustify::Center,
        TextAlignH::Right => EcJustify::Right,
        TextAlignH::Justified => EcJustify::JustifyLastLeft,
    };

    let font_size = text_data.style.font_size.max(1.0);
    let mut td = TextDoc::plain(&text_data.characters);
    td.font = text_data.style.font_family.clone();
    td.style = text_data.style.font_style.clone();
    td.size = font_size;
    td.fill = [fill_rgba.r, fill_rgba.g, fill_rgba.b, fill_rgba.a];
    td.justify = justify;

    // Position the text baseline in composition space.
    let anchor_x = match text_data.align_h {
        TextAlignH::Left | TextAlignH::Justified => decomp.top_left.x,
        TextAlignH::Center => decomp.top_left.x + node.width * 0.5,
        TextAlignH::Right => decomp.top_left.x + node.width,
    };
    let baseline_y = decomp.top_left.y + font_size * 0.82;

    if let Some(st) = layer.props.prop_mut("text/sourceText") {
        st.value = Value::Text(Box::new(td));
    }
    if let Some(tr) = layer.transform_mut() {
        set_prop_vec3(tr, "anchor", [0.0, 0.0, 0.0]);
        set_prop_vec3(tr, "position", [anchor_x, baseline_y, 0.0]);
        set_prop_vec3(tr, "scale", [decomp.scale_x * 100.0, decomp.scale_y * 100.0, 100.0]);
        set_prop_scalar(tr, "rotation", decomp.rotation_deg);
        set_prop_scalar(tr, "opacity", f64::from(node.opacity) * 100.0);
    }

    let mut ids = Ids(&mut project.next_id);
    apply_node_effects_to_layer(node, &mut layer, &mut ids, &comp.global_light, [
        f64::from(comp.width),
        f64::from(comp.height),
    ]);

    layer
}

// ----------------------------------------------------------------------------
// Paint, Stroke & Effect Conversion Helpers
// ----------------------------------------------------------------------------

fn append_fill_paints(
    node: &FcNode,
    xf: Affine,
    ids: &mut Ids,
    rule: FillRule,
    out: &mut Vec<PropGroup>,
) {
    append_paints_as_fills(&node.fills, node.width, node.height, xf, ids, rule, out);
}

fn append_paints_as_fills(
    paints: &[Paint],
    w: f64,
    h: f64,
    xf: Affine,
    ids: &mut Ids,
    rule: FillRule,
    out: &mut Vec<PropGroup>,
) {
    let rule_idx = match rule {
        FillRule::Winding => 0,
        FillRule::EvenOdd => 1,
    };
    // In FigCraft `paints`, the last entry paints on top; in EffectCraft shape contents, earlier
    // entries sit on top when `Composite = Below Previous`. Iterating in reverse puts the topmost
    // paint first.
    for paint in paints.iter().rev().filter(|p| p.visible) {
        match &paint.kind {
            PaintKind::Solid { color } => {
                let mut fill = build::shape_fill(
                    ids,
                    [
                        f64::from(color.r),
                        f64::from(color.g),
                        f64::from(color.b),
                        1.0,
                    ],
                );
                let op = f64::from(color.a * paint.opacity) * 100.0;
                set_prop_scalar(&mut fill, "opacity", op.clamp(0.0, 100.0));
                set_prop_enum(&mut fill, "rule", rule_idx);
                out.push(fill);
            }
            PaintKind::Gradient(grad) => {
                let (radial, start, end, ec_grad) = convert_gradient(grad, w, h, xf);
                let mut gfill = build::shape_gradient_fill(ids, radial, start, end, ec_grad);
                let op = f64::from(paint.opacity) * 100.0;
                set_prop_scalar(&mut gfill, "opacity", op.clamp(0.0, 100.0));
                set_prop_enum(&mut gfill, "rule", rule_idx);
                out.push(gfill);
            }
            PaintKind::Image(_) | PaintKind::Pattern(_) => {
                let c = paint.preview_color();
                let mut fill = build::shape_fill(
                    ids,
                    [f64::from(c.r), f64::from(c.g), f64::from(c.b), 1.0],
                );
                set_prop_scalar(&mut fill, "opacity", f64::from(c.a) * 100.0);
                set_prop_enum(&mut fill, "rule", rule_idx);
                out.push(fill);
            }
        }
    }
}

fn append_center_stroke_paints(
    node: &FcNode,
    xf: Affine,
    ids: &mut Ids,
    out: &mut Vec<PropGroup>,
) {
    let s = &node.stroke;
    let cap_idx = match s.cap_start {
        StrokeCap::None => 0,
        StrokeCap::Round => 1,
        StrokeCap::Square => 2,
        _ => 1,
    };
    let join_idx = match s.join {
        StrokeJoin::Miter => 0,
        StrokeJoin::Round => 1,
        StrokeJoin::Bevel => 2,
    };

    for paint in node.strokes.iter().rev().filter(|p| p.visible) {
        let mut stroke_group = match &paint.kind {
            PaintKind::Solid { color } => {
                let mut st = build::shape_stroke(
                    ids,
                    [
                        f64::from(color.r),
                        f64::from(color.g),
                        f64::from(color.b),
                        1.0,
                    ],
                    s.weight.max(0.1),
                );
                let op = f64::from(color.a * paint.opacity) * 100.0;
                set_prop_scalar(&mut st, "opacity", op.clamp(0.0, 100.0));
                st
            }
            PaintKind::Gradient(grad) => {
                let (radial, start, end, ec_grad) =
                    convert_gradient(grad, node.width, node.height, xf);
                let mut gst =
                    build::shape_gradient_stroke(ids, radial, start, end, ec_grad, s.weight.max(0.1));
                let op = f64::from(paint.opacity) * 100.0;
                set_prop_scalar(&mut gst, "opacity", op.clamp(0.0, 100.0));
                gst
            }
            PaintKind::Image(_) | PaintKind::Pattern(_) => {
                let c = paint.preview_color();
                build::shape_stroke(
                    ids,
                    [f64::from(c.r), f64::from(c.g), f64::from(c.b), 1.0],
                    s.weight.max(0.1),
                )
            }
        };

        set_prop_enum(&mut stroke_group, "cap", cap_idx);
        set_prop_enum(&mut stroke_group, "join", join_idx);
        set_prop_scalar(&mut stroke_group, "miter", s.miter_limit.clamp(1.0, 100.0));

        if let Some(&dash) = s.dashes.first()
            && dash > 0.0
        {
            let gap = s.dashes.get(1).copied().unwrap_or(dash);
            if let Some(dashes_grp) = stroke_group.sub_mut("dashes") {
                set_prop_scalar(dashes_grp, "dash", dash);
                set_prop_scalar(dashes_grp, "gap", gap);
            }
        }

        out.push(stroke_group);
    }
}

fn convert_gradient(
    grad: &FcGradient,
    w: f64,
    h: f64,
    xf: Affine,
) -> (bool, [f64; 2], [f64; 2], EcGradient) {
    let radial = matches!(
        grad.kind,
        GradientKind::Radial | GradientKind::Diamond | GradientKind::Angular
    );
    let p0 = xf * Point::new(grad.from.0 * w.max(1.0), grad.from.1 * h.max(1.0));
    let p1 = xf * Point::new(grad.to.0 * w.max(1.0), grad.to.1 * h.max(1.0));

    let stops = grad.sorted_stops();
    let colors: Vec<(f64, [f32; 4])> = stops
        .iter()
        .map(|s| {
            (
                f64::from(s.position),
                [s.color.r, s.color.g, s.color.b, 1.0],
            )
        })
        .collect();
    let opacities: Vec<(f64, f32)> = stops
        .iter()
        .map(|s| (f64::from(s.position), s.color.a))
        .collect();

    (
        radial,
        [p0.x, p0.y],
        [p1.x, p1.y],
        EcGradient { colors, opacities },
    )
}

fn apply_node_effects_to_layer(
    node: &FcNode,
    layer: &mut Layer,
    ids: &mut Ids,
    global_light: &effectcraft_project::styles::GlobalLight,
    layer_size: [f64; 2],
) {
    for fx in node.effects.iter().filter(|e| e.visible) {
        match &fx.kind {
            EffectKind::DropShadow(shadow) => {
                let _ = effectcraft_project::styles::add_style(
                    layer,
                    ids,
                    "dropShadow",
                    global_light,
                    true,
                );
                if let Some(ds) = layer
                    .props
                    .sub_mut(effectcraft_project::styles::GROUP)
                    .and_then(|ls| ls.sub_mut("dropShadow"))
                {
                    let (dist, angle_deg) = offset_to_distance_angle(shadow.offset);
                    set_prop_color(
                        ds,
                        "color",
                        [
                            f64::from(shadow.color.r),
                            f64::from(shadow.color.g),
                            f64::from(shadow.color.b),
                            1.0,
                        ],
                    );
                    set_prop_scalar(ds, "opacity", f64::from(shadow.color.a) * 100.0);
                    set_prop_bool(ds, "useGlobalLight", false);
                    set_prop_scalar(ds, "angle", angle_deg);
                    set_prop_scalar(ds, "distance", dist);
                    set_prop_scalar(ds, "size", shadow.radius.max(0.0));
                    set_prop_scalar(ds, "spread", shadow.spread.clamp(0.0, 100.0));
                }
            }
            EffectKind::InnerShadow(shadow) => {
                let _ = effectcraft_project::styles::add_style(
                    layer,
                    ids,
                    "innerShadow",
                    global_light,
                    true,
                );
                if let Some(is_grp) = layer
                    .props
                    .sub_mut(effectcraft_project::styles::GROUP)
                    .and_then(|ls| ls.sub_mut("innerShadow"))
                {
                    let (dist, angle_deg) = offset_to_distance_angle(shadow.offset);
                    set_prop_color(
                        is_grp,
                        "color",
                        [
                            f64::from(shadow.color.r),
                            f64::from(shadow.color.g),
                            f64::from(shadow.color.b),
                            1.0,
                        ],
                    );
                    set_prop_scalar(is_grp, "opacity", f64::from(shadow.color.a) * 100.0);
                    set_prop_bool(is_grp, "useGlobalLight", false);
                    set_prop_scalar(is_grp, "angle", angle_deg);
                    set_prop_scalar(is_grp, "distance", dist);
                    set_prop_scalar(is_grp, "size", shadow.radius.max(0.0));
                }
            }
            EffectKind::LayerBlur(blur) => {
                if let Some(spec) = effectcraft_effects::find("ec.blur.gaussian") {
                    let mut inst =
                        effectcraft_effects::instantiate(spec, ids, "Gaussian Blur", layer_size);
                    set_prop_scalar(&mut inst, "blurriness", blur.max_radius());
                    if let Some(effects_grp) = layer.props.sub_mut("effects") {
                        effects_grp.children.push(inst.into());
                    }
                }
            }
            _ => {}
        }
    }
}

fn offset_to_distance_angle(offset: (f64, f64)) -> (f64, f64) {
    let dist = offset.0.hypot(offset.1);
    if dist < 1e-6 {
        return (0.0, 120.0);
    }
    // In After Effects Drop Shadow, angle is direction to the light source:
    // dx = -cos(angle) * dist, dy = sin(angle) * dist.
    let angle = offset.1.atan2(-offset.0).to_degrees();
    (dist, angle)
}

// ----------------------------------------------------------------------------
// Frame Background & Transform Decomposition
// ----------------------------------------------------------------------------

fn extract_frame_background(frame: &FcNode) -> [f32; 3] {
    for paint in frame.fills.iter().rev() {
        if paint.visible {
            let c = paint.preview_color();
            return [c.r, c.g, c.b];
        }
    }
    [0.08, 0.09, 0.12]
}

fn frame_needs_background_layer(frame: &FcNode) -> bool {
    let visible_fills: Vec<&Paint> = frame.fills.iter().filter(|f| f.visible).collect();
    let has_gradient = visible_fills
        .iter()
        .any(|f| !matches!(f.kind, PaintKind::Solid { .. }));
    has_gradient || visible_fills.len() > 1 || frame.has_visible_stroke()
}

fn build_frame_background_layer(
    doc: &Document,
    frame: &FcNode,
    project: &mut Project,
    comp: &Comp,
) -> Option<Layer> {
    let mut layer = build::layer(
        project,
        comp,
        &format!("{} Background", frame.name),
        LayerSource::Shape,
        (comp.width, comp.height),
        None,
    );
    layer.comment = FRAME_BG_TAG.to_string();
    layer.label = Label::SeaFoam;
    layer.switches.shy = true;

    let mut ids = Ids(&mut project.next_id);
    let shift = Affine::translate((-frame.width * 0.5, -frame.height * 0.5));
    let mut items = Vec::new();
    items.push(build::shape_rect(
        &mut ids,
        [frame.width, frame.height],
        [0.0, 0.0],
        frame.corners.uniform().unwrap_or(0.0),
    ));
    if frame.has_visible_stroke() {
        if can_use_parametric_stroke(frame) {
            append_center_stroke_paints(frame, shift, &mut ids, &mut items);
        } else if let Some(sg) = compile_standalone_stroke_group(doc, frame, shift, &mut ids) {
            items.push(sg);
        }
    }
    append_fill_paints(frame, shift, &mut ids, FillRule::Winding, &mut items);
    let grp = build::shape_group(&mut ids, "Frame Surface", items);
    if let Some(contents) = layer.props.sub_mut("contents") {
        contents.children.push(grp.into());
    }
    Some(layer)
}

#[derive(Clone, Copy, Debug)]
struct DecomposedTransform {
    top_left: Point,
    center: Point,
    scale_x: f64,
    scale_y: f64,
    rotation_deg: f64,
}

fn decompose_affine(xf: Affine, width: f64, height: f64) -> DecomposedTransform {
    let c = xf.as_coeffs();
    let scale_x = c[0].hypot(c[1]).max(1e-6);
    let det = c[0] * c[3] - c[1] * c[2];
    let sign_y = if det < 0.0 { -1.0 } else { 1.0 };
    let scale_y = c[2].hypot(c[3]).max(1e-6) * sign_y;
    let rotation_deg = c[1].atan2(c[0]).to_degrees();
    let top_left = Point::new(c[4], c[5]);
    let center = xf * Point::new(width * 0.5, height * 0.5);
    DecomposedTransform {
        top_left,
        center,
        scale_x,
        scale_y,
        rotation_deg,
    }
}

fn apply_decomposed_transform_to_layer(
    layer: &mut Layer,
    decomp: &DecomposedTransform,
    opacity_pct: f64,
) {
    if let Some(tr) = layer.transform_mut() {
        set_prop_vec3(tr, "anchor", [0.0, 0.0, 0.0]);
        set_prop_vec3(tr, "position", [decomp.center.x, decomp.center.y, 0.0]);
        set_prop_vec3(
            tr,
            "scale",
            [decomp.scale_x * 100.0, decomp.scale_y * 100.0, 100.0],
        );
        set_prop_scalar(tr, "rotation", decomp.rotation_deg);
        set_prop_scalar(tr, "opacity", opacity_pct.clamp(0.0, 100.0));
    }
}

fn apply_affine_to_shape_group_transform(
    group: &mut PropGroup,
    xf: Affine,
    opacity_pct: f64,
) {
    let decomp = decompose_affine(xf, 0.0, 0.0);
    if let Some(tr) = group.sub_mut("transform") {
        set_prop_vec2(tr, "anchor", [0.0, 0.0]);
        set_prop_vec2(tr, "position", [decomp.top_left.x, decomp.top_left.y]);
        set_prop_vec2(tr, "scale", [decomp.scale_x * 100.0, decomp.scale_y * 100.0]);
        set_prop_scalar(tr, "rotation", decomp.rotation_deg);
        set_prop_scalar(tr, "opacity", opacity_pct.clamp(0.0, 100.0));
    }
}

fn convert_blend_mode(mode: FcBlendMode) -> EcBlendMode {
    match mode {
        FcBlendMode::PassThrough | FcBlendMode::Normal => EcBlendMode::Normal,
        FcBlendMode::Darken => EcBlendMode::Darken,
        FcBlendMode::Multiply => EcBlendMode::Multiply,
        FcBlendMode::PlusDarker | FcBlendMode::ColorBurn => EcBlendMode::ColorBurn,
        FcBlendMode::Lighten => EcBlendMode::Lighten,
        FcBlendMode::Screen => EcBlendMode::Screen,
        FcBlendMode::PlusLighter | FcBlendMode::ColorDodge => EcBlendMode::ColorDodge,
        FcBlendMode::Overlay => EcBlendMode::Overlay,
        FcBlendMode::SoftLight => EcBlendMode::SoftLight,
        FcBlendMode::HardLight => EcBlendMode::HardLight,
        FcBlendMode::Difference => EcBlendMode::Difference,
        FcBlendMode::Exclusion => EcBlendMode::Exclusion,
        FcBlendMode::Hue => EcBlendMode::Hue,
        FcBlendMode::Saturation => EcBlendMode::Saturation,
        FcBlendMode::Color => EcBlendMode::Color,
        FcBlendMode::Luminosity => EcBlendMode::Luminosity,
    }
}

fn label_for_node(node: &FcNode) -> Label {
    match &node.kind {
        NodeKind::BooleanOp { .. } => Label::Fuchsia,
        NodeKind::Vector { .. } => Label::Purple,
        NodeKind::Frame | NodeKind::Component { .. } | NodeKind::Instance { .. } => Label::Cyan,
        NodeKind::Group => Label::Yellow,
        NodeKind::Text(_) => Label::Red,
        _ => Label::Blue,
    }
}

// ----------------------------------------------------------------------------
// Animation-Preserving Re-Sync
// ----------------------------------------------------------------------------

fn reconcile_layers_preserving_animation(
    prev_comp: &Comp,
    mut new_layers: Vec<Layer>,
    preserved_count: &mut usize,
) -> Vec<Layer> {
    // Map previous FigCraft-linked layers by their node tag (`[figcraft:node=...]`).
    let mut prev_by_node: BTreeMap<u64, &Layer> = BTreeMap::new();
    let mut user_layers: Vec<(usize, Layer)> = Vec::new();

    for (idx, old_layer) in prev_comp.layers.iter().enumerate() {
        if let Some(nid) = parse_tag_u64(&old_layer.comment, NODE_TAG_PREFIX) {
            prev_by_node.insert(nid, old_layer);
        } else if old_layer.comment != GRID_OVERLAY_TAG && old_layer.comment != FRAME_BG_TAG {
            // A layer added directly in EffectCraft (e.g. Camera, Light, Adjustment Layer, Null)
            user_layers.push((idx, old_layer.clone()));
        }
    }

    for new_layer in &mut new_layers {
        let Some(nid) = parse_tag_u64(&new_layer.comment, NODE_TAG_PREFIX) else {
            continue;
        };
        let Some(&old_layer) = prev_by_node.get(&nid) else {
            continue;
        };

        // Keep stable LayerId, timing, parenting, track matte, markers, and 3D/motion-blur switches
        new_layer.id = old_layer.id;
        new_layer.start_time = old_layer.start_time;
        new_layer.in_point = old_layer.in_point;
        new_layer.out_point = old_layer.out_point;
        new_layer.stretch = old_layer.stretch;
        new_layer.parent = old_layer.parent;
        new_layer.track_matte = old_layer.track_matte;
        new_layer.markers = old_layer.markers.clone();
        new_layer.switches.motion_blur = old_layer.switches.motion_blur;
        new_layer.switches.three_d = old_layer.switches.three_d;
        new_layer.switches.solo = old_layer.switches.solo;

        let mut layer_had_anim = false;
        copy_keyframes_and_modifiers(&old_layer.props, &mut new_layer.props, &mut layer_had_anim);
        if layer_had_anim {
            *preserved_count += 1;
        }
    }

    // Re-insert any user-created EffectCraft layers at their relative position.
    for (idx, user_layer) in user_layers {
        let insert_at = idx.min(new_layers.len());
        new_layers.insert(insert_at, user_layer);
    }

    new_layers
}

fn copy_keyframes_and_modifiers(
    old_group: &PropGroup,
    new_group: &mut PropGroup,
    had_anim: &mut bool,
) {
    // 1. Copy keyframes & expressions on matching properties
    for new_child in &mut new_group.children {
        match new_child {
            EcPropNode::Prop(new_prop) => {
                if let Some(old_prop) = old_group.get(&new_prop.match_id) {
                    if !old_prop.keys.is_empty() || old_prop.expr.is_some() {
                        *had_anim = true;
                        new_prop.uid = old_prop.uid;
                        new_prop.keys = old_prop.keys.clone();
                        new_prop.expr = old_prop.expr.clone();
                    }
                }
            }
            EcPropNode::Group(new_sub) => {
                // Match by name first (for named shape groups), then by match_id
                let matching_old = old_group
                    .children
                    .iter()
                    .filter_map(EcPropNode::as_group)
                    .find(|g| g.name == new_sub.name && g.match_id == new_sub.match_id)
                    .or_else(|| old_group.sub(&new_sub.match_id));
                if let Some(old_sub) = matching_old {
                    new_sub.uid = old_sub.uid;
                    copy_keyframes_and_modifiers(old_sub, new_sub, had_anim);
                }
            }
        }
    }

    // 2. Preserve user-added shape modifiers (`trim`, `repeater`, `round`, `offset`, `pucker`,
    // `twist`, `zigzag`, `wiggle`) inside shape `contents` groups!
    if new_group.match_id == "contents" {
        for old_child in &old_group.children {
            let Some(old_sub) = old_child.as_group() else {
                continue;
            };
            let is_modifier = matches!(
                old_sub.match_id.as_str(),
                "trim" | "repeater" | "round" | "offset" | "pucker" | "twist" | "zigzag" | "wiggle"
            );
            let already_present = new_group
                .children
                .iter()
                .any(|c| c.match_id() == old_sub.match_id && c.name() == old_sub.name);
            if is_modifier && !already_present {
                *had_anim = true;
                new_group.children.push(EcPropNode::Group(old_sub.clone()));
            }
        }
    }
}

fn parse_tag_u64(comment: &str, prefix: &str) -> Option<u64> {
    let rest = comment.split(prefix).nth(1)?;
    let digits = rest.split(']').next()?;
    digits.trim().parse::<u64>().ok()
}

fn set_prop_scalar(group: &mut PropGroup, match_id: &str, val: f64) {
    if let Some(p) = group.get_mut(match_id) {
        p.value = Value::Scalar(val);
    }
}

fn set_prop_enum(group: &mut PropGroup, match_id: &str, val: u32) {
    if let Some(p) = group.get_mut(match_id) {
        p.value = Value::Enum(val);
    }
}

fn set_prop_bool(group: &mut PropGroup, match_id: &str, val: bool) {
    if let Some(p) = group.get_mut(match_id) {
        p.value = Value::Bool(val);
    }
}

fn set_prop_vec2(group: &mut PropGroup, match_id: &str, val: [f64; 2]) {
    if let Some(p) = group.get_mut(match_id) {
        p.value = Value::Vec2(val);
    }
}

fn set_prop_vec3(group: &mut PropGroup, match_id: &str, val: [f64; 3]) {
    if let Some(p) = group.get_mut(match_id) {
        p.value = Value::Vec3(val);
    }
}

fn set_prop_color(group: &mut PropGroup, match_id: &str, val: [f64; 4]) {
    if let Some(p) = group.get_mut(match_id) {
        p.value = Value::Color(val);
    }
}

// ----------------------------------------------------------------------------
// Showcase Starter Project (Design + Animate)
// ----------------------------------------------------------------------------

/// Build a rich starter `MotionStudioFile` showcasing:
/// - A 1280×720 Hero Frame with a 12-column Layout Grid and an 80px Baseline Row Grid
/// - Auto-Layout metric cards (`Horizontal` / `Vertical` auto-layout, `Fill`/`Hug` sizing, gap & padding)
/// - A custom `VectorNetwork` crest wave with a centered gradient stroke (animated with `Trim Paths`)
/// - A decorative variable-width `WidthPreset::Eye` calligraphy swoosh baked into exact contours
/// - A `BooleanOp::Subtract` emblem badge combining a Star and Ellipse
/// - Keyframed entrance animations and `Trim Paths` stroke draw-on in EffectCraft
pub fn create_showcase_studio() -> Result<MotionStudioFile, BridgeError> {
    let mut doc = Document::new("After Figs — Vector & Motion Showcase");
    let page_id = doc.first_page().unwrap_or_else(|| doc.add_page("Page 1"));
    if let Some(page) = doc.node_mut(page_id) {
        page.kind = NodeKind::Page {
            background: FcColor::from_rgb8_const(0x11, 0x13, 0x1A),
            flows: Vec::new(),
            prototype: Default::default(),
            guides: Vec::new(),
        };
    }

    // 1. Main Artboard Frame (1280 × 720) with 12-Column Grid + Row Grid
    let frame_id = doc.create(
        page_id,
        NodeKind::Frame,
        "Hero Motion Board",
        0.0,
        0.0,
        1280.0,
        720.0,
    )?;
    if let Some(frame) = doc.node_mut(frame_id) {
        frame.clip = true;
        frame.fills = vec![Paint::solid(FcColor::from_rgb8_const(0x0B, 0x0E, 0x17))];
        frame.grids = vec![
            LayoutGrid {
                kind: GridKind::Columns,
                count: 12,
                gutter: 24.0,
                offset: 64.0,
                size: 0.0,
                align: GridAlign::Stretch,
                color: FcColor::new(0.32, 0.56, 1.0, 0.08),
                visible: true,
            },
            LayoutGrid {
                kind: GridKind::Rows,
                count: 6,
                gutter: 24.0,
                offset: 48.0,
                size: 0.0,
                align: GridAlign::Stretch,
                color: FcColor::new(0.88, 0.34, 0.96, 0.06),
                visible: true,
            },
        ];
    }

    // 2. Decorative Background Glow Ring (Inside Stroke + Drop Shadow)
    let orbit_id = doc.create(
        frame_id,
        NodeKind::Ellipse {
            start: 0.0,
            sweep: std::f64::consts::TAU,
            inner: 0.0,
        },
        " Precision Orbit Ring",
        740.0,
        90.0,
        440.0,
        440.0,
    )?;
    if let Some(orbit) = doc.node_mut(orbit_id) {
        orbit.fills.clear();
        orbit.strokes = vec![Paint::gradient(FcGradient {
            kind: GradientKind::Linear,
            stops: vec![
                figcraft_color::ColorStop {
                    position: 0.0,
                    color: FcColor::from_rgb8_const(0x38, 0xBD, 0xF8),
                },
                figcraft_color::ColorStop {
                    position: 0.5,
                    color: FcColor::from_rgb8_const(0x81, 0x8C, 0xF8),
                },
                figcraft_color::ColorStop {
                    position: 1.0,
                    color: FcColor::from_rgb8_const(0xF4, 0x72, 0xB6),
                },
            ],
            from: (0.0, 0.0),
            to: (1.0, 1.0),
            width: (0.0, 1.0),
        })];
        orbit.stroke.weight = 4.0;
        orbit.stroke.align = StrokeAlign::Center;
        orbit.stroke.dashes = vec![18.0, 10.0];
    }

    // 3. Boolean Subtract Emblem ("Boolean Star Badge")
    let bool_id = doc.create(
        frame_id,
        NodeKind::BooleanOp {
            op: BoolOp::Subtract,
        },
        "Boolean Star Badge",
        870.0,
        220.0,
        180.0,
        180.0,
    )?;
    let outer_star = doc.create(
        bool_id,
        NodeKind::Star {
            count: 8,
            inner_ratio: 0.58,
        },
        "Outer Octagram",
        0.0,
        0.0,
        180.0,
        180.0,
    )?;
    if let Some(s) = doc.node_mut(outer_star) {
        s.corners.radii = [10.0; 4];
    }
    let inner_hole = doc.create(
        bool_id,
        NodeKind::Ellipse {
            start: 0.0,
            sweep: std::f64::consts::TAU,
            inner: 0.0,
        },
        "Center Cutout",
        52.0,
        52.0,
        76.0,
        76.0,
    )?;
    let _ = inner_hole;
    if let Some(b) = doc.node_mut(bool_id) {
        b.fills = vec![Paint::gradient(FcGradient {
            kind: GradientKind::Linear,
            stops: vec![
                figcraft_color::ColorStop {
                    position: 0.0,
                    color: FcColor::from_rgb8_const(0x63, 0x66, 0xF1),
                },
                figcraft_color::ColorStop {
                    position: 1.0,
                    color: FcColor::from_rgb8_const(0xEC, 0x48, 0x99),
                },
            ],
            from: (0.0, 0.0),
            to: (1.0, 1.0),
            width: (0.0, 1.0),
        })];
        b.effects = vec![figcraft_doc::Effect {
            kind: EffectKind::DropShadow(figcraft_doc::Shadow {
                color: FcColor::new(0.39, 0.40, 0.95, 0.45),
                offset: (0.0, 12.0),
                radius: 28.0,
                spread: 0.0,
                blend: FcBlendMode::Normal,
                show_behind: false,
            }),
            visible: true,
        }];
    }

    // 4. Custom VectorNetwork Signal Wave (Centered Stroke -> Trim Paths ready!)
    let mut wave_net = VectorNetwork::default();
    let wave_pts = [
        (0.0, 90.0),
        (130.0, 15.0),
        (260.0, 145.0),
        (390.0, 30.0),
        (520.0, 90.0),
    ];
    for &(x, y) in &wave_pts {
        wave_net.vertices.push(Vertex::new(x, y));
    }
    for i in 0..4 {
        wave_net.segments.push(figcraft_doc::Segment {
            a: i,
            b: i + 1,
            ta: (45.0, 0.0),
            tb: (-45.0, 0.0),
        });
    }
    let wave_id = doc.create(
        frame_id,
        NodeKind::Vector { network: wave_net },
        "Signal Spline Wave",
        64.0,
        240.0,
        520.0,
        160.0,
    )?;
    if let Some(w) = doc.node_mut(wave_id) {
        w.fills.clear();
        w.strokes = vec![Paint::gradient(FcGradient {
            kind: GradientKind::Linear,
            stops: vec![
                figcraft_color::ColorStop {
                    position: 0.0,
                    color: FcColor::from_rgb8_const(0x22, 0xD3, 0xEE),
                },
                figcraft_color::ColorStop {
                    position: 0.5,
                    color: FcColor::from_rgb8_const(0x81, 0x8C, 0xF8),
                },
                figcraft_color::ColorStop {
                    position: 1.0,
                    color: FcColor::from_rgb8_const(0xF4, 0x72, 0xB6),
                },
            ],
            from: (0.0, 0.5),
            to: (1.0, 0.5),
            width: (0.5, 1.0),
        })];
        w.stroke.weight = 6.0;
        w.stroke.align = StrokeAlign::Center;
        w.stroke.cap_start = StrokeCap::Round;
        w.stroke.cap_end = StrokeCap::Round;
        w.stroke.join = StrokeJoin::Round;
    }

    // 5. Variable-Width Tapered Ribbon (shows FigCraft WidthProfile baking!)
    let mut ribbon_net = VectorNetwork::default();
    ribbon_net.vertices.push(Vertex::new(0.0, 40.0));
    ribbon_net.vertices.push(Vertex::new(260.0, 0.0));
    ribbon_net.vertices.push(Vertex::new(520.0, 40.0));
    ribbon_net.segments.push(figcraft_doc::Segment {
        a: 0,
        b: 1,
        ta: (80.0, -20.0),
        tb: (-80.0, 0.0),
    });
    ribbon_net.segments.push(figcraft_doc::Segment {
        a: 1,
        b: 2,
        ta: (80.0, 0.0),
        tb: (-80.0, -20.0),
    });
    let ribbon_id = doc.create(
        frame_id,
        NodeKind::Vector {
            network: ribbon_net,
        },
        "Variable Width Accent Arc",
        64.0,
        175.0,
        520.0,
        55.0,
    )?;
    if let Some(r) = doc.node_mut(ribbon_id) {
        r.fills.clear();
        r.strokes = vec![Paint::solid(FcColor::from_rgb8_const(0x38, 0xBD, 0xF8))];
        r.stroke.weight = 10.0;
        r.stroke.cap_start = StrokeCap::Round;
        r.stroke.cap_end = StrokeCap::Round;
        r.stroke.width_profile = WidthPreset::Eye.profile();
    }

    // 6. Auto-Layout Row of 3 Precision Metric Cards at the bottom of the frame
    let cards_bar_id = doc.create(
        frame_id,
        NodeKind::Frame,
        "Auto-Layout Telemetry Bar",
        64.0,
        470.0,
        1152.0,
        180.0,
    )?;
    if let Some(bar) = doc.node_mut(cards_bar_id) {
        bar.fills.clear();
        bar.layout = Some(AutoLayout {
            mode: LayoutMode::Horizontal,
            gap: 24.0,
            padding: [0.0, 0.0, 0.0, 0.0],
            main_align: MainAlign::Min,
            ..AutoLayout::default()
        });
    }

    let card_specs = [
        ("Grid & Frame Snap", 0x38, 0xBD, 0xF8),
        ("Vector Networks & Booleans", 0x81, 0x8C, 0xF8),
        ("Keyframe & Expression Engine", 0xF4, 0x72, 0xB6),
    ];
    for (title, r, g, b) in card_specs {
        let card_id = doc.create(cards_bar_id, NodeKind::Frame, title, 0.0, 0.0, 368.0, 180.0)?;
        if let Some(card) = doc.node_mut(card_id) {
            card.child_layout = ChildLayout {
                sizing_h: Sizing::Fill,
                sizing_v: Sizing::Fill,
                ..ChildLayout::default()
            };
            card.corners.radii = [18.0; 4];
            card.fills = vec![Paint::solid(FcColor::from_rgba8(0x16, 0x1B, 0x2B, 235))];
            card.strokes = vec![Paint::solid(FcColor::from_rgba8(r, g, b, 180))];
            card.stroke.weight = 2.0;
            card.stroke.align = StrokeAlign::Inside;
            card.layout = Some(AutoLayout {
                mode: LayoutMode::Vertical,
                gap: 16.0,
                padding: [24.0, 24.0, 24.0, 24.0],
                ..AutoLayout::default()
            });
        }
        // Accent pill inside the auto-layout card
        let pill_id = doc.create(
            card_id,
            NodeKind::Rectangle,
            "Accent Indicator",
            0.0,
            0.0,
            96.0,
            12.0,
        )?;
        if let Some(pill) = doc.node_mut(pill_id) {
            pill.corners.radii = [6.0; 4];
            pill.fills = vec![Paint::solid(FcColor::from_rgb8_const(r, g, b))];
        }
        // Metric bar inside the auto-layout card
        let bar_line_id = doc.create(
            card_id,
            NodeKind::Rectangle,
            "Progress Track",
            0.0,
            0.0,
            280.0,
            22.0,
        )?;
        if let Some(bl) = doc.node_mut(bar_line_id) {
            bl.child_layout.sizing_h = Sizing::Fill;
            bl.corners.radii = [8.0; 4];
            bl.fills = vec![Paint::solid(FcColor::from_rgba8(r, g, b, 65))];
        }
    }

    // 7. Headline Text Node
    let mut title_text = TextData::default();
    title_text.characters = "AFTER FIGS".to_string();
    title_text.style.font_size = 44.0;
    title_text.style.font_style = "Bold".to_string();
    let title_id = doc.create(
        frame_id,
        NodeKind::Text(title_text),
        "Hero Headline",
        64.0,
        64.0,
        640.0,
        60.0,
    )?;
    if let Some(t) = doc.node_mut(title_id) {
        t.fills = vec![Paint::solid(FcColor::WHITE)];
    }

    // Bake into an EffectCraft project and enrich with starter keyframes + Trim Paths!
    let mut project = Project::default();
    let options = BakeOptions::default();
    let report = bake_frame_to_comp(&mut doc, frame_id, &mut project, &options)?;

    add_showcase_animations(&mut project, report.comp_id);

    Ok(MotionStudioFile::new(
        "After Figs Showcase",
        doc,
        project,
        options,
    ))
}

fn add_showcase_animations(project: &mut Project, comp_id: ItemId) {
    let mut next_id = project.next_id;
    if let Some(comp) = project.comp_mut(comp_id) {
        let mut ids = Ids(&mut next_id);
        let t0 = Tick::ZERO;
        let t1 = Tick::from_seconds_f64(1.2);
        let t_end = comp.duration;

        for layer in &mut comp.layers {
            if layer.name.contains("Boolean Star Badge") {
                if let Some(tr) = layer.transform_mut() {
                    if let Some(rot) = tr.get_mut("rotation") {
                        rot.keys = vec![
                            Keyframe::new(t0, Value::Scalar(0.0)).eased(),
                            Keyframe::new(t_end, Value::Scalar(180.0)).eased(),
                        ];
                    }
                    if let Some(scale) = tr.get_mut("scale") {
                        scale.keys = vec![
                            Keyframe::new(t0, Value::Vec3([75.0, 75.0, 100.0])).eased(),
                            Keyframe::new(t1, Value::Vec3([108.0, 108.0, 100.0])).eased(),
                            Keyframe::new(t_end, Value::Vec3([100.0, 100.0, 100.0])).eased(),
                        ];
                    }
                }
            } else if layer.name.contains("Orbit Ring") {
                if let Some(tr) = layer.transform_mut()
                    && let Some(rot) = tr.get_mut("rotation")
                {
                    rot.keys = vec![
                        Keyframe::new(t0, Value::Scalar(0.0)),
                        Keyframe::new(t_end, Value::Scalar(-90.0)),
                    ];
                }
            } else if layer.name.contains("Signal Spline Wave") {
                // Attach a live Trim Paths modifier to the Signal Spline Wave's shape group!
                if let Some(contents) = layer.props.sub_mut("contents") {
                    let mut trim = build::shape_trim(&mut ids, 0.0, 100.0, 0.0);
                    if let Some(end_prop) = trim.get_mut("end") {
                        end_prop.keys = vec![
                            Keyframe::new(t0, Value::Scalar(0.0)).eased(),
                            Keyframe::new(t1, Value::Scalar(100.0)).eased(),
                        ];
                    }
                    contents.children.push(trim.into());
                }
            } else if layer.name.contains("Auto-Layout Telemetry Bar") {
                if let Some(tr) = layer.transform_mut() {
                    let base_pos = tr
                        .get("position")
                        .map(|p| p.value.as_vec3())
                        .unwrap_or([640.0, 560.0, 0.0]);
                    if let Some(pos) = tr.get_mut("position") {
                        pos.keys = vec![
                            Keyframe::new(
                                t0,
                                Value::Vec3([base_pos[0], base_pos[1] + 48.0, 0.0]),
                            )
                            .eased(),
                            Keyframe::new(t1, Value::Vec3(base_pos)).eased(),
                        ];
                    }
                    if let Some(op) = tr.get_mut("opacity") {
                        op.keys = vec![
                            Keyframe::new(t0, Value::Scalar(0.0)).eased(),
                            Keyframe::new(t1, Value::Scalar(100.0)).eased(),
                        ];
                    }
                }
            }
        }
    }
    project.next_id = next_id;
}

// ----------------------------------------------------------------------------
// Unit Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn showcase_studio_bakes_grids_auto_layout_vectors_and_booleans() {
        let studio = create_showcase_studio().expect("showcase should build");
        assert_eq!(studio.frame_links.len(), 1);

        let comp_id = ItemId(*studio.frame_links.values().next().expect("linked comp"));
        let comp = studio.animation.comp(comp_id).expect("comp exists");
        assert_eq!(comp.width, 1280);
        assert_eq!(comp.height, 720);

        // LayoutGrid columns & rows should produce composition guides
        assert!(
            comp.guides.len() >= 12,
            "expected column and row guides, got {}",
            comp.guides.len()
        );

        // Guide overlay layer should be present and marked as a non-rendering guide layer
        let guide_layer = comp
            .layers
            .iter()
            .find(|l| l.comment == GRID_OVERLAY_TAG)
            .expect("guide overlay layer");
        assert!(guide_layer.switches.guide);

        // Auto-layout should have positioned the 3 telemetry cards horizontally with 24px gap
        let page_id = studio.design.first_page().expect("page");
        let frame_id = studio.design.node(page_id).expect("page").children[0];
        let bar_id = studio
            .design
            .node(frame_id)
            .expect("frame")
            .children
            .iter()
            .copied()
            .find(|&id| {
                studio
                    .design
                    .node(id)
                    .is_some_and(|n| n.name.contains("Telemetry Bar"))
            })
            .expect("telemetry bar");
        let bar = studio.design.node(bar_id).expect("bar");
        assert_eq!(bar.children.len(), 3);
        let c0 = studio.design.node(bar.children[0]).expect("card 0");
        let c1 = studio.design.node(bar.children[1]).expect("card 1");
        let c2 = studio.design.node(bar.children[2]).expect("card 2");
        assert!((c0.width - 368.0).abs() < 1.0);
        assert!((c1.transform.translation().x - (368.0 + 24.0)).abs() < 1.0);
        assert!((c2.transform.translation().x - 2.0 * (368.0 + 24.0)).abs() < 1.0);
    }

    #[test]
    fn resync_preserves_keyframes_and_trim_paths_while_updating_design() {
        let mut studio = create_showcase_studio().expect("showcase should build");
        let (&frame_u64, &comp_u64) = studio.frame_links.iter().next().expect("link");
        let frame_id = NodeId(frame_u64);
        let comp_id = ItemId(comp_u64);

        // Verify the Signal Spline Wave has Trim Paths and Boolean Star Badge has rotation keys
        {
            let comp = studio.animation.comp(comp_id).expect("comp");
            let badge = comp
                .layers
                .iter()
                .find(|l| l.name.contains("Boolean Star Badge"))
                .expect("badge");
            assert!(
                !badge
                    .transform()
                    .expect("tr")
                    .get("rotation")
                    .expect("rot")
                    .keys
                    .is_empty()
            );
            let wave = comp
                .layers
                .iter()
                .find(|l| l.name.contains("Signal Spline Wave"))
                .expect("wave");
            assert!(wave.props.sub("contents").expect("c").sub("trim").is_some());
        }

        // Now move the Boolean Star Badge in FigCraft and re-bake
        let badge_node_id = studio
            .design
            .node(frame_id)
            .expect("frame")
            .children
            .iter()
            .copied()
            .find(|&id| {
                studio
                    .design
                    .node(id)
                    .is_some_and(|n| n.name.contains("Boolean Star Badge"))
            })
            .expect("badge node");
        if let Some(badge_node) = studio.design.node_mut(badge_node_id) {
            badge_node.transform = Affine::translate((920.0, 260.0));
        }

        let report = bake_frame_to_comp(
            &mut studio.design,
            frame_id,
            &mut studio.animation,
            &studio.bake_options,
        )
        .expect("re-bake should succeed");
        assert!(report.preserved_animated_layers >= 3);

        // Verify the updated position AND preserved keyframes + Trim Paths modifier!
        let comp = studio.animation.comp(comp_id).expect("comp");
        let badge = comp
            .layers
            .iter()
            .find(|l| l.name.contains("Boolean Star Badge"))
            .expect("badge");
        let pos = badge
            .transform()
            .expect("tr")
            .get("position")
            .expect("pos")
            .value
            .as_vec3();
        assert!((pos[0] - (920.0 + 90.0)).abs() < 1e-3);
        assert!((pos[1] - (260.0 + 90.0)).abs() < 1e-3);
        assert_eq!(
            badge
                .transform()
                .expect("tr")
                .get("rotation")
                .expect("rot")
                .keys
                .len(),
            2
        );

        let wave = comp
            .layers
            .iter()
            .find(|l| l.name.contains("Signal Spline Wave"))
            .expect("wave");
        let trim = wave
            .props
            .sub("contents")
            .expect("c")
            .sub("trim")
            .expect("trim modifier preserved");
        assert_eq!(trim.get("end").expect("end").keys.len(), 2);
    }

    #[test]
    fn effectcraft_engine_renders_baked_showcase_composition() {
        let studio = create_showcase_studio().expect("showcase should build");
        let comp_id = ItemId(*studio.frame_links.values().next().expect("comp"));
        let mut session = effectcraft_engine::Session::default();
        session.project = Arc::new(studio.animation);
        let frame = session.render(
            comp_id,
            Tick::from_seconds_f64(0.6),
            effectcraft_engine::render::RenderOpts::default(),
        );
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        // Verify non-blank rendered pixels
        let non_zero = frame
            .data
            .iter()
            .filter(|px| px[0] > 0.1 || px[1] > 0.1 || px[2] > 0.1)
            .count();
        assert!(non_zero > 50_000, "expected rich rendered frame, got {non_zero}");
    }
}
