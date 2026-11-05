//! Server chrome geometry and xdg-decoration negotiation.
//!
//! Layout rectangles include the titlebar. Clients receive only the content area,
//! so maximizing a decorated window never places chrome outside the work area.
use aura_core::layout::{Rect, TrafficLightAction, WindowStateFlags};
use smithay::wayland::shell::xdg::ToplevelSurface;
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

pub const TITLEBAR_HEIGHT: f32 = 32.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Control {
    pub action: TrafficLightAction,
    pub rect: Rect,
    pub hit_rect: Rect,
    pub color: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationHit {
    Titlebar,
    Control(TrafficLightAction),
}

/// Respect explicit client chrome and never promise chrome the renderer cannot draw.
pub fn configure(surface: &ToplevelSurface, preferred: Option<Mode>, server_chrome: bool) -> Mode {
    let mode = negotiated_mode(preferred, server_chrome);
    surface.with_pending_state(|state| state.decoration_mode = Some(mode));
    surface.send_pending_configure();
    mode
}

fn negotiated_mode(preferred: Option<Mode>, server_chrome: bool) -> Mode {
    if !server_chrome || preferred == Some(Mode::ClientSide) {
        Mode::ClientSide
    } else {
        Mode::ServerSide
    }
}

/// Only negotiated server decoration receives chrome; legacy clients may draw their own.
pub fn is_server_side(mode: Option<Mode>, flags: WindowStateFlags) -> bool {
    mode == Some(Mode::ServerSide) && !flags.contains(WindowStateFlags::FULLSCREEN)
}

pub fn titlebar_rect(outer: Rect) -> Rect {
    let outer = outer.sanitized();
    Rect::new(
        outer.x,
        outer.y,
        outer.width,
        TITLEBAR_HEIGHT.min((outer.height - 1.0).max(0.0)),
    )
}

pub fn content_rect(outer: Rect, decorated: bool) -> Rect {
    let outer = outer.sanitized();
    let inset = if decorated {
        titlebar_rect(outer).height
    } else {
        0.0
    };
    Rect::new(outer.x, outer.y + inset, outer.width, outer.height - inset)
}

pub fn controls(outer: Rect) -> [Control; 3] {
    let titlebar = titlebar_rect(outer);
    let spacing = (titlebar.width / 3.0).min(22.0);
    let diameter = 12.0_f32.min((spacing - 4.0).max(0.0)).min(titlebar.height);
    let hit_width = spacing.min(20.0);
    let center_y = titlebar.y + titlebar.height * 0.5;
    let specs = [
        (TrafficLightAction::Close, [0.96, 0.30, 0.29, 1.0]),
        (TrafficLightAction::MinimizeToDock, [0.98, 0.73, 0.19, 1.0]),
        (
            TrafficLightAction::ToggleZoomFullscreen,
            [0.17, 0.78, 0.33, 1.0],
        ),
    ];
    std::array::from_fn(|index| {
        let center_x = titlebar.x + spacing * (index as f32 + 0.5);
        Control {
            action: specs[index].0,
            rect: Rect::new(
                center_x - diameter * 0.5,
                center_y - diameter * 0.5,
                diameter,
                diameter,
            ),
            hit_rect: Rect::new(
                center_x - hit_width * 0.5,
                titlebar.y,
                hit_width,
                titlebar.height,
            ),
            color: specs[index].1,
        }
    })
}

pub fn hit_test(outer: Rect, point: (f64, f64)) -> Option<DecorationHit> {
    if !contains(titlebar_rect(outer), point) {
        return None;
    }
    controls(outer)
        .into_iter()
        .find(|control| contains(control.hit_rect, point))
        .map_or(Some(DecorationHit::Titlebar), |control| {
            Some(DecorationHit::Control(control.action))
        })
}

fn contains(rect: Rect, (x, y): (f64, f64)) -> bool {
    x >= f64::from(rect.x)
        && x < f64::from(rect.x + rect.width)
        && y >= f64::from(rect.y)
        && y < f64::from(rect.y + rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_client_chrome_is_honored_and_renderer_capability_is_required() {
        assert_eq!(negotiated_mode(None, true), Mode::ServerSide);
        assert_eq!(
            negotiated_mode(Some(Mode::ServerSide), true),
            Mode::ServerSide
        );
        assert_eq!(
            negotiated_mode(Some(Mode::ClientSide), true),
            Mode::ClientSide
        );
        assert_eq!(
            negotiated_mode(Some(Mode::ServerSide), false),
            Mode::ClientSide
        );
        assert!(!is_server_side(None, WindowStateFlags::empty()));
        assert!(!is_server_side(
            Some(Mode::ClientSide),
            WindowStateFlags::empty()
        ));
        assert!(!is_server_side(
            Some(Mode::ServerSide),
            WindowStateFlags::FULLSCREEN
        ));
    }

    #[test]
    fn titlebar_and_clickable_controls_fit_inside_outer_geometry() {
        let outer = Rect::new(50.0, 80.0, 640.0, 480.0);
        assert_eq!(
            content_rect(outer, true),
            Rect::new(50.0, 112.0, 640.0, 448.0)
        );
        assert_eq!(content_rect(outer, false), outer);
        for control in controls(outer) {
            let point = (
                f64::from(control.rect.x + control.rect.width * 0.5),
                f64::from(control.rect.y + control.rect.height * 0.5),
            );
            assert_eq!(
                hit_test(outer, point),
                Some(DecorationHit::Control(control.action))
            );
        }
        assert_eq!(
            hit_test(outer, (400.0, 90.0)),
            Some(DecorationHit::Titlebar)
        );
        assert_eq!(hit_test(outer, (400.0, 112.0)), None);
        assert_eq!(hit_test(outer, (49.0, 90.0)), None);
    }

    #[test]
    fn tiny_windows_keep_nonempty_content_and_contained_hit_regions() {
        let outer = Rect::new(10.0, 20.0, 9.0, 2.0);
        assert_eq!(content_rect(outer, true).height, 1.0);
        for control in controls(outer) {
            assert!(control.hit_rect.x >= outer.x);
            assert!(control.hit_rect.x + control.hit_rect.width <= outer.x + outer.width);
        }
    }
}
