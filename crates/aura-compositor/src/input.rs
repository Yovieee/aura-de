//! `input.rs`: Slice 4 — `libinput` 3-finger touchpad swipe gestures and macOS Traffic Light hit-testing.

use aura_core::{LayoutMode, Rect, SnapZone, TrafficLightAction, WindowId};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchpadSwipeGesture {
    ThreeFingerUp,
    ThreeFingerDown,
    ThreeFingerHorizontal { delta_x: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarHit {
    TrafficLight(WindowId, TrafficLightAction),
    TitlebarDrag(WindowId),
    EdgeSnapPreview(WindowId, SnapZone),
    ClientContent(WindowId),
}

pub fn classify_three_finger_swipe(dx: f32, dy: f32, threshold: f32) -> Option<TouchpadSwipeGesture> {
    if dx.hypot(dy) < threshold {
        return None;
    }
    if dy.abs() >= dx.abs() {
        if dy < 0.0 {
            Some(TouchpadSwipeGesture::ThreeFingerUp)
        } else {
            Some(TouchpadSwipeGesture::ThreeFingerDown)
        }
    } else {
        Some(TouchpadSwipeGesture::ThreeFingerHorizontal { delta_x: dx })
    }
}

pub fn layout_mode_for_gesture(current: LayoutMode, gesture: TouchpadSwipeGesture) -> LayoutMode {
    match gesture {
        TouchpadSwipeGesture::ThreeFingerUp => LayoutMode::StageManager,
        TouchpadSwipeGesture::ThreeFingerDown => LayoutMode::Floating,
        TouchpadSwipeGesture::ThreeFingerHorizontal { .. } => match current {
            LayoutMode::DynamicTiling => LayoutMode::Floating,
            _ => LayoutMode::DynamicTiling,
        },
    }
}

pub fn hit_test_window_chrome(
    window_id: WindowId,
    win_rect: Rect,
    cursor_x: f32,
    cursor_y: f32,
    output_rect: Rect,
) -> Option<TitlebarHit> {
    if cursor_x < win_rect.x
        || cursor_x > win_rect.x + win_rect.width
        || cursor_y < win_rect.y
        || cursor_y > win_rect.y + win_rect.height
    {
        return None;
    }

    let local_x = cursor_x - win_rect.x;
    let local_y = cursor_y - win_rect.y;
    let titlebar_height = 40.0;

    if local_y <= titlebar_height {
        let button_y = 20.0;
        let hit_radius = 7.5;
        let buttons = [
            (18.0_f32, TrafficLightAction::Close),
            (38.0_f32, TrafficLightAction::MinimizeToDock),
            (58.0_f32, TrafficLightAction::ToggleZoomFullscreen),
        ];
        for (bx, action) in buttons {
            if (local_x - bx).hypot(local_y - button_y) <= hit_radius {
                return Some(TitlebarHit::TrafficLight(window_id, action));
            }
        }

        let edge_margin = 16.0;
        if cursor_x <= output_rect.x + edge_margin {
            return Some(TitlebarHit::EdgeSnapPreview(window_id, SnapZone::LeftHalf));
        }
        if cursor_x >= output_rect.x + output_rect.width - edge_margin {
            return Some(TitlebarHit::EdgeSnapPreview(window_id, SnapZone::RightHalf));
        }
        if cursor_y <= output_rect.y + 32.0 + edge_margin {
            return Some(TitlebarHit::EdgeSnapPreview(window_id, SnapZone::MaximizeFill));
        }

        return Some(TitlebarHit::TitlebarDrag(window_id));
    }

    Some(TitlebarHit::ClientContent(window_id))
}
