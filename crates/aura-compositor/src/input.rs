//! `input.rs`: Slice 4 — `libinput` 3-finger touchpad swipe gestures and Traffic Light hit-testing.

use aura_core::{LayoutMode, Rect, SnapZone, TrafficLightAction, WindowId};
use smithay::backend::input::{
    Device, Event, GestureBeginEvent, GestureEndEvent, GestureSwipeUpdateEvent, InputBackend,
    InputEvent,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchpadSwipeGesture {
    ThreeFingerUp,
    ThreeFingerDown,
    ThreeFingerHorizontal { delta_x: f32 },
}

/// A completed global gesture is emitted once, after a valid three-finger end.
/// The nested winit backend does not emit these events; native input backends
/// can drive this same dispatcher without adding a separate control path.
#[derive(Debug, Default)]
pub struct SwipeGestureState {
    active: Option<Swipe>,
}
#[derive(Debug)]
struct Swipe {
    device: String,
    dx: f64,
    dy: f64,
}
impl SwipeGestureState {
    /// Consume backend lifecycle events. Updates never commit layout changes.
    pub fn process<B: InputBackend>(
        &mut self,
        event: &InputEvent<B>,
    ) -> Option<TouchpadSwipeGesture> {
        match event {
            InputEvent::GestureSwipeBegin { event } => {
                self.active = (event.fingers() == 3).then(|| Swipe {
                    device: event.device().id(),
                    dx: 0.0,
                    dy: 0.0,
                });
            }
            InputEvent::GestureSwipeUpdate { event } => {
                let Some(active) = &mut self.active else {
                    return None;
                };
                if active.device != event.device().id() {
                    return None;
                }
                let (dx, dy) = (event.delta_x(), event.delta_y());
                if !dx.is_finite() || !dy.is_finite() || dx.abs() > 10_000.0 || dy.abs() > 10_000.0
                {
                    self.active = None;
                    return None;
                }
                active.dx = (active.dx + dx).clamp(-100_000.0, 100_000.0);
                active.dy = (active.dy + dy).clamp(-100_000.0, 100_000.0);
            }
            InputEvent::GestureSwipeEnd { event } => {
                if !self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.device == event.device().id())
                {
                    return None;
                }
                let active = self.active.take()?;
                if !event.cancelled() {
                    return classify_three_finger_swipe(active.dx as f32, active.dy as f32, 80.0);
                }
            }
            InputEvent::DeviceRemoved { device }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.device == device.id()) =>
            {
                self.active = None;
            }
            _ => {}
        }
        None
    }
}

/// Keyboard equivalents remain available on nested backends without raw swipes.
#[derive(Debug, Default)]
pub struct LayoutShortcutState {
    held: std::collections::HashSet<u32>,
}
#[derive(Debug, PartialEq)]
pub enum LayoutShortcut {
    Forward,
    Consumed,
    Apply(TouchpadSwipeGesture),
}
impl LayoutShortcutState {
    pub fn process(
        &mut self,
        code: u32,
        symbol: u32,
        pressed: bool,
        logo_only: bool,
    ) -> LayoutShortcut {
        use smithay::input::keyboard::keysyms;
        if self.held.contains(&code) {
            if !pressed {
                self.held.remove(&code);
            }
            return LayoutShortcut::Consumed;
        }
        if !pressed || !logo_only {
            return LayoutShortcut::Forward;
        }
        let gesture = match symbol {
            keysyms::KEY_Up => TouchpadSwipeGesture::ThreeFingerUp,
            keysyms::KEY_Down => TouchpadSwipeGesture::ThreeFingerDown,
            keysyms::KEY_Left | keysyms::KEY_Right => {
                TouchpadSwipeGesture::ThreeFingerHorizontal { delta_x: 0.0 }
            }
            _ => return LayoutShortcut::Forward,
        };
        self.held.insert(code);
        LayoutShortcut::Apply(gesture)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarHit {
    TrafficLight(WindowId, TrafficLightAction),
    TitlebarDrag(WindowId),
    EdgeSnapPreview(WindowId, SnapZone),
    ClientContent(WindowId),
}

pub fn classify_three_finger_swipe(
    dx: f32,
    dy: f32,
    threshold: f32,
) -> Option<TouchpadSwipeGesture> {
    if !dx.is_finite()
        || !dy.is_finite()
        || !threshold.is_finite()
        || threshold <= 0.0
        || dx.hypot(dy) < threshold
    {
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
            return Some(TitlebarHit::EdgeSnapPreview(
                window_id,
                SnapZone::MaximizeFill,
            ));
        }

        return Some(TitlebarHit::TitlebarDrag(window_id));
    }

    Some(TitlebarHit::ClientContent(window_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AuraCompositorState;
    use aura_core::GlassQualityPreset;
    use smithay::backend::input::{
        DeviceCapability, GestureSwipeBeginEvent, GestureSwipeEndEvent, UnusedEvent,
    };
    use std::path::PathBuf;

    // These events exercise the generic backend contract used by runtime input
    // dispatch. They are synthetic test data, not hardware gesture evidence.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct TestDevice(&'static str);
    impl Device for TestDevice {
        fn id(&self) -> String {
            self.0.into()
        }
        fn name(&self) -> String {
            self.0.into()
        }
        fn has_capability(&self, capability: DeviceCapability) -> bool {
            capability == DeviceCapability::Gesture
        }
        fn usb_id(&self) -> Option<(u32, u32)> {
            None
        }
        fn syspath(&self) -> Option<PathBuf> {
            None
        }
    }
    #[derive(Debug)]
    struct TestBackend;
    impl InputBackend for TestBackend {
        type Device = TestDevice;
        type KeyboardKeyEvent = UnusedEvent;
        type PointerAxisEvent = UnusedEvent;
        type PointerButtonEvent = UnusedEvent;
        type PointerMotionEvent = UnusedEvent;
        type PointerMotionAbsoluteEvent = UnusedEvent;
        type GestureSwipeBeginEvent = SwipeEvent;
        type GestureSwipeUpdateEvent = SwipeEvent;
        type GestureSwipeEndEvent = SwipeEvent;
        type GesturePinchBeginEvent = UnusedEvent;
        type GesturePinchUpdateEvent = UnusedEvent;
        type GesturePinchEndEvent = UnusedEvent;
        type GestureHoldBeginEvent = UnusedEvent;
        type GestureHoldEndEvent = UnusedEvent;
        type TouchDownEvent = UnusedEvent;
        type TouchUpEvent = UnusedEvent;
        type TouchMotionEvent = UnusedEvent;
        type TouchCancelEvent = UnusedEvent;
        type TouchFrameEvent = UnusedEvent;
        type TabletToolAxisEvent = UnusedEvent;
        type TabletToolProximityEvent = UnusedEvent;
        type TabletToolTipEvent = UnusedEvent;
        type TabletToolButtonEvent = UnusedEvent;
        type SwitchToggleEvent = UnusedEvent;
        type SpecialEvent = UnusedEvent;
    }
    #[derive(Debug)]
    struct SwipeEvent {
        device: TestDevice,
        fingers: u32,
        dx: f64,
        dy: f64,
        cancelled: bool,
    }
    impl Event<TestBackend> for SwipeEvent {
        fn time(&self) -> u64 {
            0
        }
        fn device(&self) -> TestDevice {
            self.device.clone()
        }
    }
    impl GestureBeginEvent<TestBackend> for SwipeEvent {
        fn fingers(&self) -> u32 {
            self.fingers
        }
    }
    impl GestureSwipeBeginEvent<TestBackend> for SwipeEvent {}
    impl GestureSwipeUpdateEvent<TestBackend> for SwipeEvent {
        fn delta_x(&self) -> f64 {
            self.dx
        }
        fn delta_y(&self) -> f64 {
            self.dy
        }
    }
    impl GestureEndEvent<TestBackend> for SwipeEvent {
        fn cancelled(&self) -> bool {
            self.cancelled
        }
    }
    impl GestureSwipeEndEvent<TestBackend> for SwipeEvent {}
    fn event(device: &'static str) -> SwipeEvent {
        SwipeEvent {
            device: TestDevice(device),
            fingers: 3,
            dx: 0.0,
            dy: 0.0,
            cancelled: false,
        }
    }
    fn begin(device: &'static str, fingers: u32) -> InputEvent<TestBackend> {
        InputEvent::GestureSwipeBegin {
            event: SwipeEvent {
                fingers,
                ..event(device)
            },
        }
    }
    fn update(device: &'static str, dx: f64, dy: f64) -> InputEvent<TestBackend> {
        InputEvent::GestureSwipeUpdate {
            event: SwipeEvent {
                dx,
                dy,
                ..event(device)
            },
        }
    }
    fn end(device: &'static str, cancelled: bool) -> InputEvent<TestBackend> {
        InputEvent::GestureSwipeEnd {
            event: SwipeEvent {
                cancelled,
                ..event(device)
            },
        }
    }

    #[test]
    fn backend_lifecycle_commits_geometry_only_at_end_and_only_once() {
        let mut tracker = SwipeGestureState::default();
        let mut model = AuraCompositorState::new(1280.0, 800.0, GlassQualityPreset::Balanced);
        model.map_xdg_window(1, "one", "One", (640.0, 480.0));
        model.map_xdg_window(2, "two", "Two", (640.0, 480.0));
        let before = model.workspace.windows()[0].computed_rect;
        assert!(tracker.process(&begin("pad", 3)).is_none());
        assert!(tracker.process(&update("pad", 0.0, -45.0)).is_none());
        assert!(tracker.process(&update("pad", 0.0, -45.0)).is_none());
        assert_eq!(model.workspace.mode, LayoutMode::Floating);
        let gesture = tracker.process(&end("pad", false)).unwrap();
        assert_eq!(gesture, TouchpadSwipeGesture::ThreeFingerUp);
        model.set_layout_mode(layout_mode_for_gesture(model.workspace.mode, gesture));
        assert_eq!(model.workspace.mode, LayoutMode::StageManager);
        assert_ne!(model.workspace.windows()[0].computed_rect, before);
        assert!(tracker.process(&end("pad", false)).is_none());
        assert!(tracker.process(&update("pad", 100.0, 0.0)).is_none());
    }

    #[test]
    fn cancelled_short_wrong_finger_and_invalid_sequences_never_commit() {
        for fingers in [0, 1, 2, 4, 5] {
            let mut tracker = SwipeGestureState::default();
            tracker.process(&begin("pad", fingers));
            tracker.process(&update("pad", 100.0, 0.0));
            assert!(tracker.process(&end("pad", false)).is_none());
        }
        for (dx, dy, cancelled) in [
            (79.0, 0.0, false),
            (0.0, -200.0, true),
            (f64::NAN, 0.0, false),
            (0.0, f64::INFINITY, false),
            (1e300, 0.0, false),
        ] {
            let mut tracker = SwipeGestureState::default();
            tracker.process(&begin("pad", 3));
            tracker.process(&update("pad", dx, dy));
            assert!(tracker.process(&end("pad", cancelled)).is_none());
            tracker.process(&begin("pad", 3));
            tracker.process(&update("pad", 0.0, 100.0));
            assert_eq!(
                tracker.process(&end("pad", false)),
                Some(TouchpadSwipeGesture::ThreeFingerDown)
            );
        }
    }

    #[test]
    fn device_ownership_restarts_and_unplug_prevent_cross_device_commits() {
        let mut tracker = SwipeGestureState::default();
        tracker.process(&begin("one", 3));
        tracker.process(&update("two", 100.0, 0.0));
        assert!(tracker.process(&end("two", false)).is_none());
        assert!(tracker.process(&end("one", false)).is_none());
        tracker.process(&begin("one", 3));
        tracker.process(&update("one", 100.0, 0.0));
        tracker.process(&InputEvent::<TestBackend>::DeviceRemoved {
            device: TestDevice("one"),
        });
        assert!(tracker.process(&end("one", false)).is_none());
        tracker.process(&begin("one", 3));
        tracker.process(&update("one", 100.0, 0.0));
        tracker.process(&begin("two", 3));
        tracker.process(&update("two", -100.0, 0.0));
        assert!(tracker.process(&end("one", false)).is_none());
        assert_eq!(
            tracker.process(&end("two", false)),
            Some(TouchpadSwipeGesture::ThreeFingerHorizontal { delta_x: -100.0 })
        );
    }

    #[test]
    fn shortcut_repeat_and_releases_are_consumed_even_after_modifier_changes() {
        use smithay::input::keyboard::keysyms;
        let mut keys = LayoutShortcutState::default();
        assert_eq!(
            keys.process(10, keysyms::KEY_Right, true, true),
            LayoutShortcut::Apply(TouchpadSwipeGesture::ThreeFingerHorizontal { delta_x: 0.0 })
        );
        assert_eq!(
            keys.process(10, keysyms::KEY_Right, true, true),
            LayoutShortcut::Consumed
        );
        assert_eq!(
            keys.process(10, keysyms::KEY_Right, true, false),
            LayoutShortcut::Consumed
        );
        assert_eq!(
            keys.process(10, keysyms::KEY_Right, false, false),
            LayoutShortcut::Consumed
        );
        assert_eq!(
            keys.process(10, keysyms::KEY_Right, true, false),
            LayoutShortcut::Forward
        );
    }

    #[test]
    fn normal_arrow_release_is_forwarded_when_super_was_pressed_later() {
        use smithay::input::keyboard::keysyms;
        let mut keys = LayoutShortcutState::default();
        assert_eq!(
            keys.process(10, keysyms::KEY_Up, true, false),
            LayoutShortcut::Forward
        );
        assert_eq!(
            keys.process(10, keysyms::KEY_Up, false, true),
            LayoutShortcut::Forward
        );
        assert_eq!(
            keys.process(11, keysyms::KEY_Down, true, true),
            LayoutShortcut::Apply(TouchpadSwipeGesture::ThreeFingerDown)
        );
        assert_eq!(
            keys.process(12, keysyms::KEY_a, true, true),
            LayoutShortcut::Forward
        );
    }

    #[test]
    fn classifier_rejects_nonfinite_data_and_nonpositive_thresholds() {
        assert!(classify_three_finger_swipe(f32::NAN, 100.0, 80.0).is_none());
        assert!(classify_three_finger_swipe(100.0, f32::INFINITY, 80.0).is_none());
        for threshold in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(classify_three_finger_swipe(0.0, 100.0, threshold).is_none());
        }
    }
}
