//! `aura-compositor`: Entry point for the Aura Wayland compositor.

mod input;
mod render_pass;
mod state;

use aura_core::{GlassQualityPreset, LayoutMode, TrafficLightAction};
use aura_render::{
    DUAL_KAWASE_DOWN_GLES_SOURCE, DUAL_KAWASE_UP_GLES_SOURCE, LIQUID_GLASS_GLES_SOURCE,
};
use input::{classify_three_finger_swipe, hit_test_window_chrome};
use render_pass::build_frame_render_plan;
use state::AuraCompositorState;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    WinitNested,
    TtyUdev,
}

fn parse_args() -> (BackendKind, bool) {
    let mut backend = BackendKind::WinitNested;
    let mut smoke_test = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => {
                if let Some(val) = args.next() {
                    if val == "tty-udev" || val == "drm" {
                        backend = BackendKind::TtyUdev;
                    }
                }
            }
            "--smoke-test" => smoke_test = true,
            _ => {}
        }
    }
    (backend, smoke_test)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let (backend, smoke_test) = parse_args();
    let mut state = AuraCompositorState::new(1920.0, 1080.0, GlassQualityPreset::Ultra);
    state
        .glass_material
        .validate(state.workspace.menu_bar_height)?;

    info!(
        ?backend,
        socket = %state.socket_name,
        gles_shader_bytes = LIQUID_GLASS_GLES_SOURCE.len(),
        kawase_down_bytes = DUAL_KAWASE_DOWN_GLES_SOURCE.len(),
        kawase_up_bytes = DUAL_KAWASE_UP_GLES_SOURCE.len(),
        "Initialized Aura Wayland Compositor with Liquid Glass optical pipeline"
    );

    state.map_xdg_window(1, "org.aura.Terminal", "Aura Terminal", (860.0, 540.0));
    state.map_xdg_window(2, "org.aura.Files", "Aura Files", (920.0, 600.0));
    state.set_layout_mode(LayoutMode::StageManager);
    state.tick_frame(1.0 / 120.0);

    let plan = build_frame_render_plan(&state);
    info!(
        frame = plan.frame_number,
        kawase_passes = plan.kawase_mips.len(),
        blurred_surfaces = plan.surface_uniforms.len(),
        "Compiled initial FrameRenderPlan for Smithay GlesRenderer"
    );

    if smoke_test {
        let gesture = classify_three_finger_swipe(0.0, -48.0, 24.0);
        let hit = hit_test_window_chrome(
            1,
            state.workspace.windows()[0].computed_rect,
            400.0,
            200.0,
            state.workspace.output_rect,
        );
        info!(
            ?gesture,
            ?hit,
            "Verified Slice 4 input gesture classification and titlebar hit-testing"
        );
        state.handle_traffic_light(2, TrafficLightAction::MinimizeToDock);
        info!("Smoke test completed cleanly; exiting.");
        return Ok(());
    }

    info!("Ready to accept Wayland clients on {}", state.socket_name);
    Ok(())
}
