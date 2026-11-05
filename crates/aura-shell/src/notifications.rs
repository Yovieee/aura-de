//! Drawing and matching input regions for the current notification card.
use crate::{
    client::{Canvas, PanelSpec, SupplementalPanel},
    ipc::Notification,
};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};

#[derive(Debug, PartialEq, Eq)]
pub enum Hit {
    Action(String),
    Dismiss,
    Previous,
    Next,
}

pub fn panel(revision: u64) -> SupplementalPanel {
    SupplementalPanel {
        revision,
        spec: PanelSpec {
            namespace: "aura-notifications",
            width: 360,
            height: 210,
            anchor: Anchor::TOP | Anchor::RIGHT,
            layer: Layer::Overlay,
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::None,
        },
        // Leave the menu bar and screen-sharing Stop action unobstructed.
        margins: (12, 12, 0, 12),
    }
}

fn contains(x: f64, y: f64, rect: (i32, i32, u32, u32)) -> bool {
    x >= f64::from(rect.0)
        && x < f64::from(rect.0) + f64::from(rect.2)
        && y >= f64::from(rect.1)
        && y < f64::from(rect.1) + f64::from(rect.3)
}
const DISMISS: (i32, i32, u32, u32) = (324, 8, 24, 24);
const BODY: (i32, i32, u32, u32) = (8, 38, 344, 80);
const PREVIOUS: (i32, i32, u32, u32) = (168, 166, 80, 32);
const NEXT: (i32, i32, u32, u32) = (256, 166, 92, 32);
const BUTTONS: [(i32, i32, u32, u32); 2] = [(12, 126, 150, 32), (176, 126, 172, 32)];

pub fn pages(notification: &Notification) -> usize {
    notification
        .actions
        .iter()
        .filter(|action| action.key != "default")
        .count()
        .div_ceil(2)
        .max(1)
}
pub fn hit(notification: &Notification, page: usize, x: f64, y: f64) -> Option<Hit> {
    if contains(x, y, DISMISS) {
        return Some(Hit::Dismiss);
    }
    let page = page.min(pages(notification) - 1);
    if page > 0 && contains(x, y, PREVIOUS) {
        return Some(Hit::Previous);
    }
    if page + 1 < pages(notification) && contains(x, y, NEXT) {
        return Some(Hit::Next);
    }
    for (action, rect) in notification
        .actions
        .iter()
        .filter(|action| action.key != "default")
        .skip(page * 2)
        .take(2)
        .zip(BUTTONS)
    {
        if contains(x, y, rect) {
            return Some(Hit::Action(action.key.clone()));
        }
    }
    if notification
        .actions
        .iter()
        .any(|action| action.key == "default")
        && contains(x, y, BODY)
    {
        return Some(Hit::Action("default".into()));
    }
    None
}
fn button(canvas: &mut Canvas<'_>, rect: (i32, i32, u32, u32), label: &str) {
    canvas.rect(rect.0, rect.1, rect.2, rect.3, 0xff365d84);
    let label: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take((rect.2.saturating_sub(16) / 8) as usize)
        .collect();
    canvas.text(rect.0 + 8, rect.1 + 12, &label, 0xffedf3fc, 1);
}
pub fn draw(notification: &Notification, page: usize, canvas: &mut Canvas<'_>) {
    canvas.clear(0xff192534);
    let summary: String = notification
        .summary
        .chars()
        .filter(|c| !c.is_control())
        .take(37)
        .collect();
    let app: String = notification
        .app
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    canvas.text(12, 16, &summary, 0xffedf3fc, 1);
    canvas.text(12, 42, &app, 0xff9bcaff, 1);
    let body: Vec<_> = notification
        .body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(126)
        .collect();
    for (line, chunk) in body.chunks(42).enumerate() {
        canvas.text(
            12,
            66 + line as i32 * 16,
            &chunk.iter().collect::<String>(),
            0xffc6d3e3,
            1,
        );
    }
    button(canvas, DISMISS, "X");
    let page = page.min(pages(notification) - 1);
    for (action, rect) in notification
        .actions
        .iter()
        .filter(|action| action.key != "default")
        .skip(page * 2)
        .take(2)
        .zip(BUTTONS)
    {
        button(canvas, rect, &action.label);
    }
    if pages(notification) > 1 {
        canvas.text(
            12,
            178,
            &format!("Actions {}/{}", page + 1, pages(notification)),
            0xffb8c8dc,
            1,
        );
        if page > 0 {
            button(canvas, PREVIOUS, "Previous");
        }
        if page + 1 < pages(notification) {
            button(canvas, NEXT, "Next");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::NotificationAction;
    fn notification() -> Notification {
        Notification {
            id: 1,
            generation: 7,
            owner: ":1.42".into(),
            app: "Probe".into(),
            summary: "Summary".into(),
            body: "Body".into(),
            expires: None,
            resident: false,
            actions: ["default", "one", "two", "three"]
                .map(|key| NotificationAction {
                    key: key.into(),
                    label: key.into(),
                })
                .into(),
        }
    }
    #[test]
    fn every_action_has_a_page_and_default_is_separate() {
        let notification = notification();
        assert_eq!(pages(&notification), 2);
        assert_eq!(
            hit(&notification, 0, 90., 142.),
            Some(Hit::Action("one".into()))
        );
        assert_eq!(
            hit(&notification, 0, 200., 142.),
            Some(Hit::Action("two".into()))
        );
        assert_eq!(hit(&notification, 0, 300., 182.), Some(Hit::Next));
        assert_eq!(
            hit(&notification, 1, 90., 142.),
            Some(Hit::Action("three".into()))
        );
        assert_eq!(hit(&notification, 1, 200., 182.), Some(Hit::Previous));
        assert_eq!(hit(&notification, 1, 300., 182.), None);
        assert_eq!(
            hit(&notification, 0, 20., 60.),
            Some(Hit::Action("default".into()))
        );
        assert_eq!(hit(&notification, 0, 336., 20.), Some(Hit::Dismiss));
        assert_eq!(hit(&notification, 0, 350., 142.), None);
    }
    #[test]
    fn missing_default_or_actions_never_invents_a_target() {
        let mut notification = notification();
        notification.actions.clear();
        assert_eq!(pages(&notification), 1);
        assert_eq!(hit(&notification, usize::MAX, 20., 60.), None);
        assert_eq!(hit(&notification, 0, 90., 142.), None);
        assert_eq!(hit(&notification, 0, 336., 20.), Some(Hit::Dismiss));
    }
}
