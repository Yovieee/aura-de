//! Opaque SHM fallback panel and Dock, with live D-Bus notifications.
use aura_shell::{
    catalog::DesktopCatalog,
    client::{self, Canvas, DesktopUi, PanelSpec},
    desktop::{self, DesktopEntryItem},
    dock::{self, DockLayout},
    history::{self, DetailHit, HistoryEntry, HistoryRow, ListHit},
    ipc::{self, SharedState},
    notifications::{self, Hit},
};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use std::sync::{Arc, Mutex, mpsc};
#[derive(Clone, Default, serde::Deserialize)]
struct Window {
    id: u64,
    app_id: String,
    title: String,
    minimized: bool,
    focused: bool,
}
#[derive(Default)]
struct RuntimeState {
    windows: Vec<Window>,
    status: String,
    sharing: usize,
}
#[derive(serde::Deserialize)]
struct CompositorStatus {
    capture_subscribers: usize,
}
enum Action {
    Control(String),
    Launch(DesktopEntryItem),
    Launcher,
    Settings,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
enum NotificationView {
    #[default]
    Auto,
    Hidden,
    Active {
        id: u32,
        generation: u64,
    },
    History {
        page: usize,
        selected: usize,
    },
    Detail {
        entry: HistoryEntry,
        page: usize,
        return_page: usize,
        return_selected: usize,
    },
}
#[derive(Clone)]
enum DrawnHistory {
    List {
        rows: Vec<HistoryRow>,
        page: usize,
        selected: usize,
    },
    Detail {
        entry: HistoryEntry,
        page: usize,
        return_page: usize,
        return_selected: usize,
    },
}
fn current_notification<'a>(
    state: &'a ipc::ShellState,
    view: &NotificationView,
) -> Option<&'a ipc::Notification> {
    match view {
        NotificationView::Auto if !state.do_not_disturb => {
            state.notifications.values().max_by_key(|n| n.generation)
        }
        NotificationView::Active { id, generation } => state
            .notifications
            .get(id)
            .filter(|n| n.generation == *generation),
        _ => None,
    }
}
fn reconcile_view(view: &mut NotificationView, state: &ipc::ShellState, seen: &mut u64) {
    let latest = state
        .notifications
        .values()
        .map(|n| n.generation)
        .max()
        .unwrap_or(*seen);
    if latest > *seen {
        *seen = latest;
        if matches!(view, NotificationView::Hidden) && !state.do_not_disturb {
            *view = NotificationView::Auto;
        }
    }
    if let NotificationView::Active { id, generation } = view {
        if let Some(notification) = state.notifications.get(id) {
            *generation = notification.generation;
        } else {
            *view = NotificationView::Auto;
        }
    }
}
/// Keyboard selection advances immediately, even when several keys precede a draw.
/// Row identities still come from the last rendered snapshot and are revalidated on open.
fn history_key(view: &mut NotificationView, rows: &[HistoryRow], key: u32) -> Option<ListHit> {
    let NotificationView::History { page, selected } = view else {
        return None;
    };
    *page = (*page).min(history::list_pages(rows) - 1);
    *selected = (*selected).min(rows.len().saturating_sub(1));
    let next = match key {
        0xff1b => return Some(ListHit::Close),
        0xff52 => selected.saturating_sub(1),
        0xff54 => (*selected + 1).min(rows.len().saturating_sub(1)),
        0xff51 => page.saturating_sub(1) * 6,
        0xff53 => ((*page + 1).min(history::list_pages(rows) - 1)) * 6,
        0xff0d | 0xff8d => {
            return rows.get(*selected).map(|row| match row {
                HistoryRow::Active { id, generation, .. } => ListHit::OpenActive {
                    id: *id,
                    generation: *generation,
                },
                HistoryRow::Archived(entry) => ListHit::OpenArchived {
                    serial: entry.serial,
                },
            });
        }
        _ => return None,
    };
    *selected = next;
    *page = next / 6;
    None
}
struct Shell {
    state: SharedState,
    runtime: Arc<Mutex<RuntimeState>>,
    apps: Arc<Vec<DesktopEntryItem>>,
    drawn_apps: Arc<Vec<DesktopEntryItem>>,
    drawn_windows: Vec<u64>,
    catalog: DesktopCatalog,
    catalog_revision: u64,
    dock_hover: Option<(f64, f64)>,
    dock_layout: DockLayout,
    actions: mpsc::Sender<Action>,
    redraw_fingerprint: Option<u64>,
    menu_width: u32,
    settings_rect: Option<dock::Rect>,
    notification_menu_rect: Option<dock::Rect>,
    notification_view: NotificationView,
    latest_notification_seen: u64,
    history_request_revision: u64,
    drawn_history: Option<DrawnHistory>,
    notification_key: Option<(u32, u64)>,
    notification_page: usize,
    drawn_notification: Option<ipc::Notification>,
    drawn_notification_page: usize,
}
impl Shell {
    fn history_hit(&mut self, hit: ListHit, page: usize, selected: usize) {
        match hit {
            ListHit::OpenActive { id, generation } => {
                if self
                    .state
                    .lock()
                    .unwrap()
                    .notifications
                    .get(&id)
                    .is_some_and(|n| n.generation == generation)
                {
                    self.notification_view = NotificationView::Active { id, generation };
                }
            }
            ListHit::OpenArchived { serial } => {
                if let Some(entry) = self
                    .state
                    .lock()
                    .unwrap()
                    .history
                    .entries()
                    .iter()
                    .find(|entry| entry.serial == serial)
                    .cloned()
                {
                    self.notification_view = NotificationView::Detail {
                        entry,
                        page: 0,
                        return_page: page,
                        return_selected: selected,
                    };
                }
            }
            ListHit::Clear => {
                self.state.lock().unwrap().history.clear();
                self.notification_view = NotificationView::History {
                    page: 0,
                    selected: 0,
                };
            }
            ListHit::Previous => {
                self.notification_view = NotificationView::History {
                    page: page.saturating_sub(1),
                    selected: page.saturating_sub(1) * 6,
                }
            }
            ListHit::Next => {
                self.notification_view = NotificationView::History {
                    page: page + 1,
                    selected: (page + 1) * 6,
                }
            }
            ListHit::Close => self.notification_view = NotificationView::Hidden,
        }
    }
    fn detail_hit(&mut self, hit: DetailHit, drawn: DrawnHistory) {
        let DrawnHistory::Detail {
            entry,
            page,
            return_page,
            return_selected,
        } = drawn
        else {
            return;
        };
        self.notification_view = match hit {
            DetailHit::Back => NotificationView::History {
                page: return_page,
                selected: return_selected,
            },
            DetailHit::Close => NotificationView::Hidden,
            DetailHit::Previous | DetailHit::Next => NotificationView::Detail {
                page: if hit == DetailHit::Previous {
                    page.saturating_sub(1)
                } else {
                    (page + 1).min(history::detail_pages(&entry) - 1)
                },
                entry,
                return_page,
                return_selected,
            },
        };
    }
}
impl DesktopUi for Shell {
    fn needs_redraw(&mut self) -> bool {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let snapshot = self.catalog.snapshot();
        self.apps = snapshot.entries;
        self.catalog_revision = snapshot.revision;
        let mut hash = DefaultHasher::new();
        self.catalog_revision.hash(&mut hash);
        self.dock_hover
            .map(|(x, y)| (x.to_bits(), y.to_bits()))
            .hash(&mut hash);
        {
            let state = self.state.lock().unwrap();
            state.reduced_transparency.hash(&mut hash);
            state.do_not_disturb.hash(&mut hash);
            reconcile_view(
                &mut self.notification_view,
                &state,
                &mut self.latest_notification_seen,
            );
            state.history.revision().hash(&mut hash);
            for n in state.notifications.values() {
                (n.id, n.generation).hash(&mut hash);
            }
            let notification = current_notification(&state, &self.notification_view);
            let key = notification.map(|n| (n.id, n.generation));
            if self.notification_key != key {
                self.notification_key = key;
                self.notification_page = 0;
            }
            self.notification_view.hash(&mut hash);
            self.notification_page.hash(&mut hash);
        }
        {
            let runtime = self.runtime.lock().unwrap();
            runtime.status.hash(&mut hash);
            runtime.sharing.hash(&mut hash);
            for window in runtime.windows.iter().take(6) {
                (
                    window.id,
                    &window.app_id,
                    &window.title,
                    window.minimized,
                    window.focused,
                )
                    .hash(&mut hash);
            }
        }
        let fingerprint = hash.finish();
        let changed = self.redraw_fingerprint != Some(fingerprint);
        self.redraw_fingerprint = Some(fingerprint);
        changed
    }
    fn tick_interval(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(100))
    }
    fn supplemental_panels(&self) -> Vec<client::SupplementalPanel> {
        if matches!(
            self.notification_view,
            NotificationView::History { .. } | NotificationView::Detail { .. }
        ) {
            return vec![history::panel(self.history_request_revision)];
        }
        let state = self.state.lock().unwrap();
        current_notification(&state, &self.notification_view)
            .map(|n| vec![notifications::panel(n.generation)])
            .unwrap_or_default()
    }

    fn draw(&mut self, panel: usize, c: &mut Canvas<'_>) {
        if panel == 2 {
            self.drawn_notification = None;
            self.drawn_history = None;
            let state = self.state.lock().unwrap();
            match &self.notification_view {
                NotificationView::History { page, selected } => {
                    let rows = history::rows(&state.notifications, &state.history);
                    let page = (*page).min(history::list_pages(&rows) - 1);
                    let selected = (*selected).min(rows.len().saturating_sub(1));
                    history::draw_list_selected(&rows, page, state.do_not_disturb, selected, c);
                    self.drawn_history = Some(DrawnHistory::List {
                        rows,
                        page,
                        selected,
                    });
                }
                NotificationView::Detail {
                    entry,
                    page,
                    return_page,
                    return_selected,
                } => {
                    let page = (*page).min(history::detail_pages(entry) - 1);
                    history::draw_detail(entry, page, c);
                    self.drawn_history = Some(DrawnHistory::Detail {
                        entry: entry.clone(),
                        page,
                        return_page: *return_page,
                        return_selected: *return_selected,
                    });
                }
                _ => {
                    self.drawn_notification =
                        current_notification(&state, &self.notification_view).cloned();
                    if let Some(notification) = &self.drawn_notification {
                        self.drawn_notification_page = self
                            .notification_page
                            .min(notifications::pages(notification) - 1);
                        notifications::draw(notification, self.drawn_notification_page, c);
                    } else {
                        c.clear(0);
                    }
                }
            }
            return;
        }
        let reduced = self.state.lock().unwrap().reduced_transparency;
        c.panel_background(0xff182230, reduced);
        if panel == 0 {
            self.menu_width = c.width;
            c.text(
                12,
                10,
                "AURA  |  Floating   Stage Manager   Tiling",
                0xffe1e9f5,
                1,
            );
            let state = self.state.lock().unwrap();
            let sharing = self.runtime.lock().unwrap().sharing > 0;
            let label = if state.do_not_disturb {
                "DND"
            } else {
                "Notifications"
            };
            let notification_x = c.width.saturating_sub(if sharing { 380 } else { 180 });
            self.notification_menu_rect = (notification_x >= 484).then_some(dock::Rect {
                x: notification_x as f32,
                y: 4.,
                width: 172.,
                height: 24.,
            });
            if let Some(rect) = self.notification_menu_rect {
                c.rect(rect.x as i32, 4, 172, 24, 0xff2a3c51);
                c.text(
                    rect.x as i32 + 4,
                    12,
                    &format!(
                        "{label}: {}",
                        state.notifications.len() + state.history.entries().len()
                    ),
                    0xffb8c8dc,
                    1,
                );
            }
            let settings_x = c.width.saturating_sub(if sharing { 492 } else { 292 });
            self.settings_rect = (settings_x >= 372).then_some(dock::Rect {
                x: settings_x as f32,
                y: 4.,
                width: 104.,
                height: 24.,
            });
            if let Some(rect) = self.settings_rect {
                c.rect(
                    rect.x as i32,
                    rect.y as i32,
                    rect.width as u32,
                    rect.height as u32,
                    0xff2a3c51,
                );
                c.text(rect.x as i32 + 12, 12, "Settings", 0xffedf3fc, 1);
            }
            if sharing {
                let x = c.width.saturating_sub(192) as i32;
                c.rect(x, 4, 184, 24, 0xffa12835);
                c.text(x + 12, 12, "SHARING | Stop", 0xffffffff, 1);
            }
        } else {
            let runtime = self.runtime.lock().unwrap();
            self.dock_layout = dock::layout(
                c.width,
                c.height,
                self.apps.len(),
                runtime.windows.len(),
                self.dock_hover,
            );
            self.drawn_apps = self.apps.clone();
            self.drawn_windows = runtime
                .windows
                .iter()
                .take(6)
                .map(|window| window.id)
                .collect();
            if let Some(rect) = self.dock_layout.launcher {
                c.rect(
                    rect.x as i32,
                    rect.y as i32,
                    rect.width as u32,
                    rect.height as u32,
                    0xff223449,
                );
                c.text(rect.x as i32 + 4, rect.y as i32 + 12, "Apps", 0xff9bcaff, 1);
            }
            for item in &self.dock_layout.apps {
                let app = &self.drawn_apps[item.index];
                let rect = item.rect;
                c.rect(
                    rect.x as i32,
                    rect.y as i32,
                    rect.width as u32,
                    rect.height as u32,
                    0xff28384c,
                );
                let letters = (rect.width as usize).saturating_sub(8) / 8;
                c.text(
                    rect.x as i32 + 4,
                    (rect.y + rect.height * 0.5 - 4.) as i32,
                    &app.name.chars().take(letters).collect::<String>(),
                    0xffedf2f9,
                    1,
                );
            }
            for item in &self.dock_layout.windows {
                let window = &runtime.windows[item.index];
                let rect = item.rect;
                c.rect(
                    rect.x as i32,
                    rect.y as i32,
                    rect.width as u32,
                    rect.height as u32,
                    if window.focused {
                        0xff3b6084
                    } else {
                        0xff28384c
                    },
                );
                let title = if window.title.is_empty() {
                    &window.app_id
                } else {
                    &window.title
                };
                let letters = (rect.width as usize).saturating_sub(6) / 8;
                c.text(
                    rect.x as i32 + 3,
                    rect.y as i32 + 6,
                    &format!("{}{}", if window.minimized { "- " } else { "" }, title)
                        .chars()
                        .take(letters)
                        .collect::<String>(),
                    0xffe1e9f5,
                    1,
                );
            }
            let notification = {
                let state = self.state.lock().unwrap();
                if state.do_not_disturb {
                    None
                } else {
                    current_notification(&state, &self.notification_view).cloned()
                }
            };
            if let Some(n) = notification {
                c.text(12, 94, &format!("{}: {}", n.app, n.summary), 0xffa9d5ff, 1);
                c.text(12, 103, &n.body, 0xffb8c8dc, 1);
            } else {
                c.text(12, 94, &runtime.status, 0xffb8c8dc, 1);
            }
        }
    }
    fn pointer_leave(&mut self, panel: usize) {
        if panel == 1 {
            self.dock_hover = None;
        }
    }
    fn key(&mut self, key: u32, _: Option<&str>) -> bool {
        match self.drawn_history.clone() {
            Some(DrawnHistory::List { rows, .. })
                if matches!(self.notification_view, NotificationView::History { .. }) =>
            {
                if let Some(hit) = history_key(&mut self.notification_view, &rows, key)
                    && let NotificationView::History { page, selected } = self.notification_view
                {
                    self.history_hit(hit, page, selected);
                }
            }
            Some(DrawnHistory::Detail { .. }) => {
                if let NotificationView::Detail {
                    entry,
                    page,
                    return_page,
                    return_selected,
                } = self.notification_view.clone()
                {
                    let hit = match key {
                        0xff1b | 0xff08 => DetailHit::Back,
                        0xff51 => DetailHit::Previous,
                        0xff53 => DetailHit::Next,
                        _ => return false,
                    };
                    self.detail_hit(
                        hit,
                        DrawnHistory::Detail {
                            entry,
                            page,
                            return_page,
                            return_selected,
                        },
                    );
                }
            }
            _ => {}
        }
        false
    }
    fn pointer(&mut self, panel: usize, x: f64, y: f64, pressed: bool) {
        if panel == 1 {
            self.dock_hover = Some((x, y));
        }
        if !pressed {
            return;
        }
        if panel == 2 {
            if let Some(drawn) = self.drawn_history.clone() {
                match &drawn {
                    DrawnHistory::List {
                        rows,
                        page,
                        selected,
                    } if matches!(self.notification_view, NotificationView::History { .. }) => {
                        if let Some(hit) = history::list_hit(rows, *page, x, y) {
                            self.history_hit(hit, *page, *selected);
                        }
                    }
                    DrawnHistory::Detail { entry, page, .. }
                        if matches!(self.notification_view, NotificationView::Detail { .. }) =>
                    {
                        if let Some(hit) = history::detail_hit(entry, *page, x, y) {
                            self.detail_hit(hit, drawn);
                        }
                    }
                    _ => {}
                }
                return;
            }
            if let Some(notification) = &self.drawn_notification
                && self.notification_key == Some((notification.id, notification.generation))
            {
                match notifications::hit(notification, self.drawn_notification_page, x, y) {
                    Some(Hit::Action(key)) => {
                        ipc::enqueue_action(
                            &self.state,
                            notification.id,
                            notification.generation,
                            &key,
                        );
                    }
                    Some(Hit::Dismiss) => {
                        ipc::enqueue_dismiss(&self.state, notification.id, notification.generation);
                    }
                    Some(Hit::Previous) => {
                        self.notification_page = self.drawn_notification_page.saturating_sub(1)
                    }
                    Some(Hit::Next) => {
                        self.notification_page = (self.drawn_notification_page + 1)
                            .min(notifications::pages(notification) - 1)
                    }
                    None => {}
                }
            }
            return;
        }
        let action = if panel == 0 {
            if self
                .notification_menu_rect
                .is_some_and(|rect| rect.contains(x, y))
            {
                self.notification_view = if matches!(
                    self.notification_view,
                    NotificationView::History { .. } | NotificationView::Detail { .. }
                ) {
                    NotificationView::Hidden
                } else {
                    self.history_request_revision = self.history_request_revision.wrapping_add(1);
                    NotificationView::History {
                        page: 0,
                        selected: 0,
                    }
                };
                return;
            }
            if self.settings_rect.is_some_and(|rect| rect.contains(x, y)) {
                let _ = self.actions.send(Action::Settings);
                return;
            }
            if self.runtime.lock().unwrap().sharing > 0
                && (f64::from(self.menu_width.saturating_sub(192))
                    ..f64::from(self.menu_width.saturating_sub(8)))
                    .contains(&x)
                && (4.0..28.0).contains(&y)
            {
                let _ = self
                    .actions
                    .send(Action::Control("stop-screen-sharing".into()));
                return;
            }
            let mode = if (80.0..160.0).contains(&x) {
                Some("floating")
            } else if (168.0..280.0).contains(&x) {
                Some("stage-manager")
            } else if (296.0..360.0).contains(&x) {
                Some("tiling")
            } else {
                None
            };
            mode.map(|m| Action::Control(format!("layout {m}")))
        } else if self
            .dock_layout
            .launcher
            .is_some_and(|rect| rect.contains(x, y))
        {
            Some(Action::Launcher)
        } else if let Some(item) = self
            .dock_layout
            .apps
            .iter()
            .find(|item| item.rect.contains(x, y))
        {
            self.drawn_apps
                .get(item.index)
                .filter(|app| self.apps.contains(app))
                .cloned()
                .map(Action::Launch)
        } else if let Some(item) = self
            .dock_layout
            .windows
            .iter()
            .find(|item| item.rect.contains(x, y))
        {
            self.drawn_windows
                .get(item.index)
                .map(|id| Action::Control(format!("restore {id}")))
        } else {
            None
        };
        if let Some(action) = action {
            let _ = self.actions.send(action);
        }
    }
}
fn open_panel(name: &str) -> Result<String, String> {
    let program = std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name(name))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| name.into());
    std::process::Command::new(program)
        .spawn()
        .map(|mut child| {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            format!("{name} opened")
        })
        .map_err(|error| error.to_string())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let state = SharedState::default();
    state.lock().unwrap().reduced_transparency =
        std::env::var("AURA_GLASS_QUALITY").is_ok_and(|value| value == "reduced-transparency");
    ipc::start_services_with_preferences(state.clone()).map_err(std::io::Error::other)?;
    let runtime = Arc::new(Mutex::new(RuntimeState::default()));
    let worker_state = runtime.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (completed, results) = mpsc::sync_channel::<Result<String, String>>(4);
        let mut pending = 0usize;
        loop {
            while let Ok(result) = results.try_recv() {
                pending = pending.saturating_sub(1);
                worker_state.lock().unwrap().status =
                    result.unwrap_or_else(|error| format!("Error: {error}"));
            }
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(action) => {
                    let result = match action {
                        Action::Control(c) => ipc::control_command(&c),
                        Action::Launch(app) => {
                            if pending >= 4 {
                                Err("Four applications are still starting; try again shortly"
                                    .into())
                            } else {
                                let name = app.name.clone();
                                let completed = completed.clone();
                                match std::thread::Builder::new()
                                    .name("aura-dock-launch".into())
                                    .spawn(move || {
                                        let result = desktop::launch(&app)
                                            .map(|_| format!("Started {}", app.name));
                                        let _ = completed.send(result);
                                    }) {
                                    Ok(_) => {
                                        pending += 1;
                                        Ok(format!("Starting {name}"))
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            }
                        }
                        Action::Launcher => open_panel("aura-launcher"),
                        Action::Settings => open_panel("aura-settings"),
                    };
                    worker_state.lock().unwrap().status =
                        result.unwrap_or_else(|e| format!("Error: {e}"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if let Ok(json) = ipc::control_command("windows")
                && let Ok(windows) = serde_json::from_str(&json)
            {
                worker_state.lock().unwrap().windows = windows;
            }
            if let Ok(json) = ipc::control_command("status")
                && let Ok(status) = serde_json::from_str::<CompositorStatus>(&json)
            {
                worker_state.lock().unwrap().sharing = status.capture_subscribers;
            }
        }
    });
    let specs = vec![
        PanelSpec {
            namespace: "aura-menu-bar",
            width: 0,
            height: 32,
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            layer: Layer::Top,
            exclusive_zone: 32,
            keyboard: KeyboardInteractivity::None,
        },
        PanelSpec {
            namespace: "aura-dock",
            width: 720,
            height: 112,
            anchor: Anchor::BOTTOM,
            layer: Layer::Top,
            exclusive_zone: 112,
            keyboard: KeyboardInteractivity::None,
        },
    ];
    let mut catalog = DesktopCatalog::new()?;
    let snapshot = catalog.snapshot();
    client::run(
        specs,
        Box::new(Shell {
            state,
            runtime,
            apps: snapshot.entries.clone(),
            drawn_apps: snapshot.entries,
            drawn_windows: Vec::new(),
            catalog,
            catalog_revision: snapshot.revision,
            dock_hover: None,
            dock_layout: DockLayout::default(),
            actions: tx,
            redraw_fingerprint: None,
            menu_width: 0,
            settings_rect: None,
            notification_menu_rect: None,
            notification_view: NotificationView::Auto,
            latest_notification_seen: 0,
            history_request_revision: 0,
            drawn_history: None,
            notification_key: None,
            notification_page: 0,
            drawn_notification: None,
            drawn_notification_page: 0,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: u32, generation: u64) -> ipc::Notification {
        ipc::Notification {
            id,
            generation,
            owner: ":1.2".into(),
            actions: Vec::new(),
            resident: false,
            app: "App".into(),
            summary: "Message".into(),
            body: "Body".into(),
            expires: None,
        }
    }

    #[test]
    fn replacement_is_newest_and_explicit_history_can_open_an_older_live_card_in_dnd() {
        let mut state = ipc::ShellState::default();
        state.notifications.insert(1, notification(1, 3));
        state.notifications.insert(2, notification(2, 2));
        assert_eq!(
            current_notification(&state, &NotificationView::Auto)
                .unwrap()
                .id,
            1
        );
        state.do_not_disturb = true;
        assert!(current_notification(&state, &NotificationView::Auto).is_none());
        assert_eq!(
            current_notification(
                &state,
                &NotificationView::Active {
                    id: 2,
                    generation: 2
                }
            )
            .unwrap()
            .id,
            2
        );
        assert!(
            current_notification(
                &state,
                &NotificationView::Active {
                    id: 2,
                    generation: 1
                }
            )
            .is_none()
        );
    }

    #[test]
    fn removing_newest_does_not_reopen_a_hidden_card_but_a_new_arrival_does() {
        let mut state = ipc::ShellState::default();
        state.notifications.insert(1, notification(1, 1));
        let mut view = NotificationView::Hidden;
        let mut seen = 2;
        reconcile_view(&mut view, &state, &mut seen);
        assert_eq!(view, NotificationView::Hidden);
        assert_eq!(seen, 2);
        state.notifications.insert(3, notification(3, 3));
        reconcile_view(&mut view, &state, &mut seen);
        assert_eq!(view, NotificationView::Auto);
    }

    #[test]
    fn rapid_keyboard_selection_and_paging_accumulate_before_the_next_draw() {
        let rows: Vec<_> = (1..=13)
            .map(|id| HistoryRow::Active {
                id,
                generation: u64::from(id),
                app: "App".into(),
                summary: "Message".into(),
            })
            .collect();
        let mut view = NotificationView::History {
            page: 0,
            selected: 0,
        };
        assert_eq!(history_key(&mut view, &rows, 0xff54), None);
        assert_eq!(
            history_key(&mut view, &rows, 0xff0d),
            Some(ListHit::OpenActive {
                id: 2,
                generation: 2
            })
        );
        history_key(&mut view, &rows, 0xff53);
        history_key(&mut view, &rows, 0xff53);
        assert_eq!(
            history_key(&mut view, &rows, 0xff0d),
            Some(ListHit::OpenActive {
                id: 13,
                generation: 13
            })
        );
        assert_eq!(history_key(&mut view, &[], 0xff0d), None);
        assert_eq!(history_key(&mut view, &[], 0xff1b), Some(ListHit::Close));
    }

    #[test]
    fn new_arrivals_do_not_interrupt_history_and_selected_replacements_refresh_the_token() {
        let mut state = ipc::ShellState::default();
        state.notifications.insert(1, notification(1, 4));
        let mut view = NotificationView::History {
            page: 1,
            selected: 7,
        };
        let mut seen = 1;
        reconcile_view(&mut view, &state, &mut seen);
        assert_eq!(
            view,
            NotificationView::History {
                page: 1,
                selected: 7
            }
        );
        view = NotificationView::Active {
            id: 1,
            generation: 1,
        };
        reconcile_view(&mut view, &state, &mut seen);
        assert_eq!(
            view,
            NotificationView::Active {
                id: 1,
                generation: 4
            }
        );
        state.notifications.clear();
        reconcile_view(&mut view, &state, &mut seen);
        assert_eq!(view, NotificationView::Auto);
    }
}
