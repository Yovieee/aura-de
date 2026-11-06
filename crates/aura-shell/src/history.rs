//! Bounded session-only notification history and read-only history views.
//!
//! An archived entry deliberately contains neither sender identity nor action
//! keys. Only an explicitly live row can request its current notification card.
use crate::{
    client::{Canvas, PanelSpec, SupplementalPanel},
    ipc::Notification,
};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use std::collections::{BTreeMap, VecDeque};

pub const HISTORY_LIMIT: usize = 100;
const ROWS_PER_PAGE: usize = 6;
const BODY_COLUMNS: usize = 40;
const BODY_LINES_PER_PAGE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HistoryEntry {
    pub serial: u64,
    pub app: String,
    pub summary: String,
    pub body: String,
    pub reason: u32,
}

#[derive(Debug, Default)]
pub struct NotificationHistory {
    entries: VecDeque<HistoryEntry>,
    next_serial: u64,
    revision: u64,
}

fn clipped(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

impl NotificationHistory {
    /// Archive only after the caller has committed an actual close or expiry.
    /// Entries are newest first and never retain an actionable notification ID.
    pub fn archive(&mut self, notification: &Notification, reason: u32) -> u64 {
        loop {
            self.next_serial = self.next_serial.wrapping_add(1).max(1);
            if !self
                .entries
                .iter()
                .any(|entry| entry.serial == self.next_serial)
            {
                break;
            }
        }
        let serial = self.next_serial;
        self.entries.push_front(HistoryEntry {
            serial,
            app: clipped(&notification.app, 128),
            summary: clipped(&notification.summary, 256),
            body: clipped(&notification.body, 2048),
            reason,
        });
        self.entries.truncate(HISTORY_LIMIT);
        self.revision = self.revision.wrapping_add(1);
        serial
    }

    pub fn entries(&self) -> &VecDeque<HistoryEntry> {
        &self.entries
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Clearing history leaves live notifications to their owning service.
    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HistoryRow {
    Active {
        id: u32,
        generation: u64,
        app: String,
        summary: String,
    },
    Archived(HistoryEntry),
}

/// A UI snapshot: active notifications by newest generation, then closed ones.
pub fn rows(
    active: &BTreeMap<u32, Notification>,
    history: &NotificationHistory,
) -> Vec<HistoryRow> {
    let mut current: Vec<_> = active.values().collect();
    current.sort_by_key(|notification| std::cmp::Reverse(notification.generation));
    current
        .into_iter()
        .map(|notification| HistoryRow::Active {
            id: notification.id,
            generation: notification.generation,
            app: clipped(&notification.app, 128),
            summary: clipped(&notification.summary, 256),
        })
        .chain(history.entries.iter().cloned().map(HistoryRow::Archived))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListHit {
    OpenActive { id: u32, generation: u64 },
    OpenArchived { serial: u64 },
    Clear,
    Previous,
    Next,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailHit {
    Back,
    Previous,
    Next,
    Close,
}

pub fn panel(revision: u64) -> SupplementalPanel {
    SupplementalPanel {
        revision,
        spec: PanelSpec {
            namespace: "aura-notification-history",
            width: 360,
            height: 420,
            anchor: Anchor::TOP | Anchor::RIGHT,
            layer: Layer::Overlay,
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::Exclusive,
        },
        margins: (12, 12, 0, 12),
    }
}

type Rect = (i32, i32, u32, u32);
const CLOSE: Rect = (324, 8, 24, 24);
const CLEAR: Rect = (12, 344, 148, 32);
const BACK: Rect = (12, 380, 148, 28);
const PREVIOUS: Rect = (168, 380, 80, 28);
const NEXT: Rect = (256, 380, 92, 28);

fn row_bounds(index: usize) -> Rect {
    (12, 72 + index as i32 * 44, 336, 40)
}

fn contains(x: f64, y: f64, rect: Rect) -> bool {
    x >= f64::from(rect.0)
        && y >= f64::from(rect.1)
        && x < f64::from(rect.0) + f64::from(rect.2)
        && y < f64::from(rect.1) + f64::from(rect.3)
}

pub fn list_pages(rows: &[HistoryRow]) -> usize {
    rows.len().div_ceil(ROWS_PER_PAGE).max(1)
}

/// Resolve against the rows/page actually drawn, never a newly reordered list.
pub fn list_hit(rows: &[HistoryRow], page: usize, x: f64, y: f64) -> Option<ListHit> {
    let page = page.min(list_pages(rows) - 1);
    if contains(x, y, CLOSE) {
        return Some(ListHit::Close);
    }
    if rows
        .iter()
        .any(|row| matches!(row, HistoryRow::Archived(_)))
        && contains(x, y, CLEAR)
    {
        return Some(ListHit::Clear);
    }
    if page > 0 && contains(x, y, PREVIOUS) {
        return Some(ListHit::Previous);
    }
    if page + 1 < list_pages(rows) && contains(x, y, NEXT) {
        return Some(ListHit::Next);
    }
    for (index, row) in rows
        .iter()
        .skip(page * ROWS_PER_PAGE)
        .take(ROWS_PER_PAGE)
        .enumerate()
    {
        if contains(x, y, row_bounds(index)) {
            return Some(match row {
                HistoryRow::Active { id, generation, .. } => ListHit::OpenActive {
                    id: *id,
                    generation: *generation,
                },
                HistoryRow::Archived(entry) => ListHit::OpenArchived {
                    serial: entry.serial,
                },
            });
        }
    }
    None
}

pub fn reason_label(reason: u32) -> &'static str {
    match reason {
        1 => "Expired",
        2 => "Closed by user",
        3 => "Closed by app",
        _ => "Closed",
    }
}

fn label(text: &str, count: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(count)
        .collect()
}

fn button(canvas: &mut Canvas<'_>, rect: Rect, text: &str) {
    canvas.rect(rect.0, rect.1, rect.2, rect.3, 0xff365d84);
    canvas.text(
        rect.0 + 8,
        rect.1 + 10,
        &label(text, ((rect.2 - 16) / 8) as usize),
        0xffedf3fc,
        1,
    );
}

pub fn draw_list(rows: &[HistoryRow], page: usize, dnd: bool, canvas: &mut Canvas<'_>) {
    draw_list_selected(rows, page, dnd, page.saturating_mul(ROWS_PER_PAGE), canvas);
}

/// `selected` is an absolute row index, so keyboard selection survives paging.
pub fn draw_list_selected(
    rows: &[HistoryRow],
    page: usize,
    dnd: bool,
    selected: usize,
    canvas: &mut Canvas<'_>,
) {
    canvas.clear(0xff192534);
    canvas.text(12, 16, "Notifications", 0xffedf3fc, 2);
    button(canvas, CLOSE, "X");
    canvas.text(
        12,
        46,
        if dnd {
            "Do not disturb: On"
        } else {
            "Do not disturb: Off"
        },
        0xff9bcaff,
        1,
    );
    let active = rows
        .iter()
        .filter(|row| matches!(row, HistoryRow::Active { .. }))
        .count();
    canvas.text(
        12,
        60,
        &format!("{active} active | {} in history", rows.len() - active),
        0xffb8c8dc,
        1,
    );
    let page = page.min(list_pages(rows) - 1);
    if rows.is_empty() {
        canvas.text(20, 92, "No notifications in this session", 0xffc6d3e3, 1);
    }
    for (index, row) in rows
        .iter()
        .skip(page * ROWS_PER_PAGE)
        .take(ROWS_PER_PAGE)
        .enumerate()
    {
        let (x, y, width, height) = row_bounds(index);
        let (kind, app, summary) = match row {
            HistoryRow::Active { app, summary, .. } => ("Active", app, summary),
            HistoryRow::Archived(entry) => (reason_label(entry.reason), &entry.app, &entry.summary),
        };
        if page * ROWS_PER_PAGE + index == selected {
            canvas.rect(x - 2, y - 2, width + 4, height + 4, 0xff9bcaff);
        }
        canvas.rect(
            x,
            y,
            width,
            height,
            if matches!(row, HistoryRow::Active { .. }) {
                0xff2a3c51
            } else {
                0xff202f40
            },
        );
        canvas.text(
            x + 8,
            y + 7,
            &label(&format!("{kind} | {app}"), 40),
            0xff9bcaff,
            1,
        );
        canvas.text(x + 8, y + 23, &label(summary, 40), 0xffedf3fc, 1);
    }
    if rows
        .iter()
        .any(|row| matches!(row, HistoryRow::Archived(_)))
    {
        button(canvas, CLEAR, "Clear history");
    } else {
        canvas.text(12, 356, "History is empty", 0xffb8c8dc, 1);
    }
    canvas.text(
        180,
        356,
        &format!("Page {}/{}", page + 1, list_pages(rows)),
        0xffb8c8dc,
        1,
    );
    if page > 0 {
        button(canvas, PREVIOUS, "Previous");
    }
    if page + 1 < list_pages(rows) {
        button(canvas, NEXT, "Next");
    }
}

fn detail_lines(entry: &HistoryEntry) -> Vec<String> {
    let text = format!("{}\n\n{}", entry.summary, entry.body);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let chars: Vec<_> = paragraph
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if chars.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(
                chars
                    .chunks(BODY_COLUMNS)
                    .map(|chunk| chunk.iter().collect()),
            );
        }
    }
    lines
}

pub fn detail_pages(entry: &HistoryEntry) -> usize {
    detail_lines(entry)
        .len()
        .div_ceil(BODY_LINES_PER_PAGE)
        .max(1)
}

/// Archived content has navigation only; no coordinate can return a live action.
pub fn detail_hit(entry: &HistoryEntry, page: usize, x: f64, y: f64) -> Option<DetailHit> {
    let page = page.min(detail_pages(entry) - 1);
    if contains(x, y, CLOSE) {
        return Some(DetailHit::Close);
    }
    if contains(x, y, BACK) {
        return Some(DetailHit::Back);
    }
    if page > 0 && contains(x, y, PREVIOUS) {
        return Some(DetailHit::Previous);
    }
    if page + 1 < detail_pages(entry) && contains(x, y, NEXT) {
        return Some(DetailHit::Next);
    }
    None
}

pub fn draw_detail(entry: &HistoryEntry, page: usize, canvas: &mut Canvas<'_>) {
    canvas.clear(0xff192534);
    canvas.text(12, 16, "Notification", 0xffedf3fc, 2);
    button(canvas, CLOSE, "X");
    canvas.text(12, 46, &label(&entry.app, 40), 0xff9bcaff, 1);
    canvas.text(12, 62, reason_label(entry.reason), 0xffb8c8dc, 1);
    let page = page.min(detail_pages(entry) - 1);
    for (line, text) in detail_lines(entry)
        .iter()
        .skip(page * BODY_LINES_PER_PAGE)
        .take(BODY_LINES_PER_PAGE)
        .enumerate()
    {
        canvas.text(12, 84 + line as i32 * 16, text, 0xffedf3fc, 1);
    }
    canvas.text(
        12,
        352,
        &format!("Read only | Page {}/{}", page + 1, detail_pages(entry)),
        0xffb8c8dc,
        1,
    );
    button(canvas, BACK, "Back");
    if page > 0 {
        button(canvas, PREVIOUS, "Previous");
    }
    if page + 1 < detail_pages(entry) {
        button(canvas, NEXT, "Next");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::NotificationAction;

    fn notification(id: u32, generation: u64) -> Notification {
        Notification {
            id,
            generation,
            owner: ":1.42".into(),
            app: "Sample".into(),
            summary: format!("Message {id}"),
            body: "Body".into(),
            actions: vec![NotificationAction {
                key: "launch-old-action".into(),
                label: "Open".into(),
            }],
            resident: false,
            expires: None,
        }
    }

    #[test]
    fn retention_is_bounded_newest_first_and_serials_survive_clear() {
        let mut history = NotificationHistory::default();
        for id in 0..105 {
            history.archive(&notification(id, u64::from(id) + 1), 1);
        }
        assert_eq!(history.entries().len(), 100);
        assert_eq!(history.entries().front().unwrap().summary, "Message 104");
        assert_eq!(history.entries().back().unwrap().summary, "Message 5");
        let snapshot = history.entries().front().unwrap().clone();
        let revision = history.revision();
        history.clear();
        assert!(history.entries().is_empty());
        assert_ne!(history.revision(), revision);
        assert!(history.archive(&notification(105, 106), 3) > snapshot.serial);
        assert_eq!(snapshot.summary, "Message 104");
    }

    #[test]
    fn archive_clips_by_unicode_characters_and_keeps_only_read_only_content() {
        let mut source = notification(1, 4);
        source.app = "🦀".repeat(200);
        source.summary = "é".repeat(300);
        source.body = "界".repeat(3000);
        let mut history = NotificationHistory::default();
        let serial = history.archive(&source, 2);
        let entry = history.entries().front().unwrap();
        assert_eq!(entry.app.chars().count(), 128);
        assert_eq!(entry.summary.chars().count(), 256);
        assert_eq!(entry.body.chars().count(), 2048);
        assert_eq!(entry.reason, 2);
        assert!(detail_pages(entry) > 1);
        assert_eq!(detail_hit(entry, 0, 20., 120.), None);
        let display_rows = rows(&BTreeMap::new(), &history);
        assert_eq!(
            list_hit(&display_rows, 0, 30., 90.),
            Some(ListHit::OpenArchived { serial })
        );
        assert_eq!(detail_hit(entry, 0, 80., 394.), Some(DetailHit::Back));
    }

    #[test]
    fn rendered_active_tokens_do_not_retarget_a_replacement_or_new_order() {
        let mut active = BTreeMap::new();
        active.insert(1, notification(1, 20));
        active.insert(2, notification(2, 5));
        let history = NotificationHistory::default();
        let snapshot = rows(&active, &history);
        assert_eq!(
            list_hit(&snapshot, 0, 30., 90.),
            Some(ListHit::OpenActive {
                id: 1,
                generation: 20
            })
        );
        active.insert(1, notification(1, 21));
        active.insert(3, notification(3, 22));
        assert_eq!(
            list_hit(&snapshot, 0, 30., 90.),
            Some(ListHit::OpenActive {
                id: 1,
                generation: 20
            })
        );
        assert_eq!(
            list_hit(&rows(&active, &history), 0, 30., 90.),
            Some(ListHit::OpenActive {
                id: 3,
                generation: 22
            })
        );
        assert_eq!(list_hit(&snapshot, 0, 80., 360.), None);
    }

    #[test]
    fn every_archived_row_is_reachable_and_clear_does_not_touch_active_entries() {
        let mut history = NotificationHistory::default();
        for id in 1..9 {
            history.archive(&notification(id, u64::from(id)), 1);
        }
        let mut active = BTreeMap::new();
        active.insert(50, notification(50, 50));
        let snapshot = rows(&active, &history);
        assert_eq!(list_pages(&snapshot), 2);
        assert_eq!(list_hit(&snapshot, 0, 300., 394.), Some(ListHit::Next));
        assert_eq!(list_hit(&snapshot, 1, 208., 394.), Some(ListHit::Previous));
        assert_eq!(list_hit(&snapshot, 1, 300., 394.), None);
        assert_eq!(list_hit(&snapshot, 0, 80., 360.), Some(ListHit::Clear));
        for (index, row) in snapshot.iter().enumerate().skip(1) {
            let HistoryRow::Archived(entry) = row else {
                panic!("closed entry must be read-only")
            };
            assert_eq!(
                list_hit(
                    &snapshot,
                    index / 6,
                    30.,
                    f64::from(80 + (index % 6) as i32 * 44)
                ),
                Some(ListHit::OpenArchived {
                    serial: entry.serial
                })
            );
        }
        history.clear();
        assert_eq!(active.len(), 1);
        assert_eq!(rows(&active, &history).len(), 1);
    }
}
