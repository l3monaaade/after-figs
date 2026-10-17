//! MotionCraft Studio Web Application (`wasm32-unknown-unknown`).
//!
//! Runs the unified FigCraft (Design) + EffectCraft (Animate) studio inside an HTML5 `<canvas>`
//! using `eframe::WebRunner` (WebGPU with WebGL2 fallback).

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast as _;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Start the MotionCraft Studio web application inside the `<canvas>` element with `canvas_id`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn start(canvas_id: String) -> Result<(), JsValue> {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();

    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let canvas = document
        .get_element_by_id(&canvas_id)
        .ok_or_else(|| JsValue::from_str("missing canvas element"))?
        .dyn_into::<web_sys::HtmlCanvasElement>()?;

    let mut options = eframe::WebOptions::default();
    let search = window.location().search().unwrap_or_default();
    if search.contains("webgl")
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) =
            &mut options.wgpu_options.wgpu_setup
    {
        create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
    }

    let result = eframe::WebRunner::new()
        .start(
            canvas,
            options,
            Box::new(|cc| {
                if let Some(rs) = &cc.wgpu_render_state {
                    log::info!(
                        "motion-web: initialized wgpu backend {:?}",
                        rs.adapter.get_info().backend
                    );
                }
                Ok(Box::new(motion_ui::MotionStudioApp::new_with_showcase()))
            }),
        )
        .await;

    if let Some(loading) = document.get_element_by_id("motion_loading") {
        match &result {
            Ok(()) => loading.remove(),
            Err(e) => {
                loading.set_inner_html(&format!(
                    "<div style='text-align:center;max-width:480px;padding:24px'>\
                     <h3 style='color:#f87171;margin-bottom:8px'>MotionCraft Studio failed to start</h3>\
                     <p style='color:#94a3b8;font-size:13px'>{e:?}</p>\
                     <p style='color:#64748b;font-size:12px'>Ensure WebGPU or WebGL2 is enabled in your browser.</p>\
                     </div>"
                ));
            }
        }
    }

    result
}
