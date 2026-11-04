//! Opaque SHM fallback panel and Dock, with live D-Bus notifications.
use aura_shell::{
    client::{self, Canvas, DesktopUi, PanelSpec},
    desktop::{self, DesktopEntryItem},
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
}
enum Action {
    Control(String),
    Launch(DesktopEntryItem),
    Launcher,
}
struct Shell {
    state: SharedState,
    runtime: Arc<Mutex<RuntimeState>>,
    apps: Vec<DesktopEntryItem>,
    actions: mpsc::Sender<Action>,
}
impl DesktopUi for Shell {
    fn draw(&mut self, panel: usize, c: &mut Canvas<'_>) {
        let reduced = self.state.lock().unwrap().reduced_transparency;
        c.panel_background(0xff182230, reduced);
        if panel == 0 {
            c.text(
                12,
                10,
                "AURA  |  Floating   Stage Manager   Tiling",
                0xffe1e9f5,
                1,
            );
            let state = self.state.lock().unwrap();
            let label = if state.do_not_disturb {
                "DND"
            } else {
                "Notifications"
            };
            c.text(
                c.width.saturating_sub(180) as i32,
                10,
                &format!("{label}: {}", state.notifications.len()),
                0xffb8c8dc,
                1,
            );
        } else {
            c.text(12, 12, "Apps", 0xff9bcaff, 1);
            for (i, app) in self.apps.iter().take(5).enumerate() {
                let x = 76 + i as i32 * 104;
                c.rect(x, 8, 96, 42, 0xff28384c);
                c.text(
                    x + 4,
                    24,
                    &app.name.chars().take(11).collect::<String>(),
                    0xffedf2f9,
                    1,
                );
            }
            let runtime = self.runtime.lock().unwrap();
            for (i, w) in runtime.windows.iter().take(6).enumerate() {
                let x = 12 + i as i32 * 112;
                c.rect(
                    x,
                    58,
                    106,
                    26,
                    if w.focused { 0xff3b6084 } else { 0xff28384c },
                );
                c.text(
                    x + 3,
                    67,
                    &format!(
                        "{}{}",
                        if w.minimized { "- " } else { "" },
                        if w.title.is_empty() {
                            &w.app_id
                        } else {
                            &w.title
                        }
                    )
                    .chars()
                    .take(12)
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
    fn pointer(&mut self, panel: usize, x: f64, y: f64, pressed: bool) {
        if !pressed {
            return;
        }
        let action = if panel == 0 {
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
        } else if y < 54.0 {
            if x < 72.0 {
                Some(Action::Launcher)
            } else {
                self.apps
                    .get(((x - 76.0) / 104.0).max(0.0) as usize)
                    .cloned()
                    .map(Action::Launch)
            }
        } else if y < 88.0 {
            self.runtime
                .lock()
                .unwrap()
                .windows
                .get(((x - 12.0) / 112.0).max(0.0) as usize)
                .map(|w| Action::Control(format!("restore {}", w.id)))
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
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(action) => {
                    let result = match action {
                        Action::Control(c) => ipc::control_command(&c),
                        Action::Launch(app) => {
                            desktop::launch(&app).map(|_| format!("Started {}", app.name))
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
    client::run(
        specs,
        Box::new(Shell {
            state,
            runtime,
            apps: desktop::discover(),
            actions: tx,
        }),
    )
}
