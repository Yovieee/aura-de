//! Opaque SHM fallback panel and Dock, with live D-Bus notifications.
use aura_shell::{
    catalog::DesktopCatalog,
    client::{self, Canvas, DesktopUi, PanelSpec},
    desktop::{self, DesktopEntryItem},
    dock::{self, DockLayout},
    ipc::{self, SharedState},
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
            state.notifications.len().hash(&mut hash);
            if let Some(notification) = state.notifications.values().next_back() {
                (&notification.app, &notification.summary, &notification.body).hash(&mut hash);
            }
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
    fn draw(&mut self, panel: usize, c: &mut Canvas<'_>) {
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
            c.text(
                c.width.saturating_sub(if sharing { 380 } else { 180 }) as i32,
                10,
                &format!("{label}: {}", state.notifications.len()),
                0xffb8c8dc,
                1,
            );
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
                    state.notifications.values().next_back().cloned()
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
    fn pointer(&mut self, panel: usize, x: f64, y: f64, pressed: bool) {
        if panel == 1 {
            self.dock_hover = Some((x, y));
        }
        if !pressed {
            return;
        }
        let action = if panel == 0 {
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
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let state = SharedState::default();
    state.lock().unwrap().reduced_transparency =
        std::env::var("AURA_GLASS_QUALITY").is_ok_and(|value| value == "reduced-transparency");
    ipc::start_services(state.clone()).map_err(std::io::Error::other)?;
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
                        Action::Launcher => {
                            let sibling = std::env::current_exe()
                                .ok()
                                .map(|p| p.with_file_name("aura-launcher"));
                            let program = sibling
                                .filter(|p| p.is_file())
                                .unwrap_or_else(|| "aura-launcher".into());
                            std::process::Command::new(program)
                                .spawn()
                                .map(|mut child| {
                                    std::thread::spawn(move || {
                                        let _ = child.wait();
                                    });
                                    "Launcher opened".into()
                                })
                                .map_err(|e| e.to_string())
                        }
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
        }),
    )
}
