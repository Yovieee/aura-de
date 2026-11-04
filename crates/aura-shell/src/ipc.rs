//! Live session D-Bus services and compositor control transport.
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zbus::{interface, object_server::SignalEmitter, zvariant::OwnedValue};

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub body: String,
    pub expires: Option<Instant>,
}
#[derive(Default)]
pub struct ShellState {
    pub do_not_disturb: bool,
    pub reduced_transparency: bool,
    pub notifications: BTreeMap<u32, Notification>,
    next_id: u32,
}
pub type SharedState = Arc<Mutex<ShellState>>;
impl ShellState {
    fn notify(&mut self, app: &str, replaces: u32, summary: &str, body: &str, timeout: i32) -> u32 {
        let id = if replaces != 0 && self.notifications.contains_key(&replaces) {
            replaces
        } else {
            loop {
                self.next_id = self.next_id.wrapping_add(1).max(1);
                if !self.notifications.contains_key(&self.next_id) {
                    break self.next_id;
                }
            }
        };
        let timeout = if timeout < 0 { 5000 } else { timeout };
        self.notifications.insert(
            id,
            Notification {
                id,
                app: app.into(),
                summary: summary.into(),
                body: body.into(),
                expires: (timeout > 0)
                    .then(|| Instant::now() + Duration::from_millis(timeout as u64)),
            },
        );
        id
    }
    fn expired(&mut self, now: Instant) -> Vec<u32> {
        let ids: Vec<_> = self
            .notifications
            .values()
            .filter(|n| n.expires.is_some_and(|e| e <= now))
            .map(|n| n.id)
            .collect();
        for id in &ids {
            self.notifications.remove(id);
        }
        ids
    }
}
pub fn control_path() -> Result<PathBuf, String> {
    std::env::var_os("AURA_CONTROL_SOCKET")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR").map(|p| PathBuf::from(p).join("aura-control.sock"))
        })
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set".into())
}
pub fn control_command(command: &str) -> Result<String, String> {
    if command.contains(['\n', '\r']) {
        return Err("Invalid control command".into());
    }
    let mut socket = UnixStream::connect(control_path()?).map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    writeln!(socket, "{command}").map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(socket)
        .take(1024 * 1024)
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    if response.is_empty() || response.starts_with("ERR") {
        return Err(response.trim().to_owned());
    }
    Ok(response.trim().to_owned())
}
async fn command(command: String) -> zbus::fdo::Result<String> {
    tokio::task::spawn_blocking(move || control_command(&command))
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        .map_err(zbus::fdo::Error::Failed)
}
pub struct AuraShellService {
    pub state: SharedState,
}
#[interface(name = "org.aura.Shell1")]
impl AuraShellService {
    async fn set_layout_mode(&self, mode: &str) -> zbus::fdo::Result<String> {
        if !["floating", "stage-manager", "tiling"].contains(&mode) {
            return Err(zbus::fdo::Error::InvalidArgs("Unknown layout mode".into()));
        }
        command(format!("layout {mode}")).await
    }
    async fn set_glass_quality(&self, preset: &str) -> zbus::fdo::Result<String> {
        if !["ultra", "balanced", "reduced-transparency"].contains(&preset) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Unknown quality preset".into(),
            ));
        }
        let response = command(format!("quality {preset}")).await?;
        self.state.lock().unwrap().reduced_transparency = preset == "reduced-transparency";
        Ok(response)
    }
    async fn focus_window(&self, id: u64) -> zbus::fdo::Result<String> {
        command(format!("restore {id}")).await
    }
    async fn list_windows(&self) -> zbus::fdo::Result<String> {
        command("windows".into()).await
    }
    #[zbus(property)]
    fn do_not_disturb(&self) -> bool {
        self.state.lock().unwrap().do_not_disturb
    }
    #[zbus(property)]
    fn set_do_not_disturb(&self, value: bool) {
        self.state.lock().unwrap().do_not_disturb = value;
    }
}
pub struct NotificationService {
    pub state: SharedState,
}
#[interface(name = "org.freedesktop.Notifications")]
impl NotificationService {
    fn get_capabilities(&self) -> Vec<&str> {
        vec!["body"]
    }
    fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("Aura Shell", "Aura DE", env!("CARGO_PKG_VERSION"), "1.2")
    }
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        _hints: std::collections::HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        self.state
            .lock()
            .unwrap()
            .notify(app_name, replaces_id, summary, body, expire_timeout)
    }
    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let removed = self
            .state
            .lock()
            .unwrap()
            .notifications
            .remove(&id)
            .is_some();
        if removed {
            Self::notification_closed(&emitter, id, 3)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        }
        Ok(())
    }
    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}
/// Starts D-Bus services on a dedicated runtime and waits for registration.
pub fn start_services(state: SharedState) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        runtime.block_on(async move {
            let result = async {
                zbus::connection::Builder::session()?
                    .name("org.aura.Shell1")?
                    .name("org.freedesktop.Notifications")?
                    .serve_at(
                        "/org/aura/Shell1",
                        AuraShellService {
                            state: state.clone(),
                        },
                    )?
                    .serve_at(
                        "/org/freedesktop/Notifications",
                        NotificationService {
                            state: state.clone(),
                        },
                    )?
                    .build()
                    .await
            }
            .await;
            let connection = match result {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = tx.send(Ok(()));
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let expired = state.lock().unwrap().expired(Instant::now());
                if let Ok(interface) = connection
                    .object_server()
                    .interface::<_, NotificationService>("/org/freedesktop/Notifications")
                    .await
                {
                    for id in expired {
                        let _ = NotificationService::notification_closed(
                            interface.signal_emitter(),
                            id,
                            1,
                        )
                        .await;
                    }
                }
            }
        });
    });
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|e| e.to_string())?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_ids_and_expiration() {
        let mut s = ShellState::default();
        let a = s.notify("app", 0, "first", "body", 100);
        let b = s.notify("app", a, "replacement", "", 0);
        assert_eq!(a, b);
        assert_eq!(s.notifications.len(), 1);
        assert_eq!(s.notifications[&a].summary, "replacement");
        assert!(
            s.expired(Instant::now() + Duration::from_secs(1))
                .is_empty()
        );
        let c = s.notify("app", 0, "expires", "", 1);
        assert_ne!(c, a);
        assert_eq!(s.expired(Instant::now() + Duration::from_secs(1)), vec![c]);
        assert!(s.notifications.contains_key(&a));
    }
}
