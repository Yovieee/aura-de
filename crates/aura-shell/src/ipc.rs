//! Live session D-Bus services and compositor control transport.
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zbus::{interface, message::Header, object_server::SignalEmitter, zvariant::OwnedValue};

const NOTIFICATION_PATH: &str = "/org/freedesktop/Notifications";
const MAX_ACTIONS: usize = 8;
const MAX_PENDING_INTENTS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationAction {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    /// Changes on every replacement; UI input must target the rendered version.
    pub generation: u64,
    pub owner: String,
    pub actions: Vec<NotificationAction>,
    pub resident: bool,
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
    next_generation: u64,
    intents: VecDeque<NotificationIntent>,
    // Serialize mutations with signal emission without holding the UI mutex across await.
    notification_gate: Arc<tokio::sync::Mutex<()>>,
}
#[derive(Debug)]
enum NotificationIntent {
    Action {
        id: u32,
        generation: u64,
        key: String,
    },
    Dismiss {
        id: u32,
        generation: u64,
    },
}
#[derive(Debug, PartialEq, Eq)]
enum NotificationEvent {
    Action { id: u32, owner: String, key: String },
    Closed { id: u32, owner: String, reason: u32 },
}
pub type SharedState = Arc<Mutex<ShellState>>;
impl ShellState {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        owner: &str,
        app: &str,
        replaces: u32,
        summary: &str,
        body: &str,
        actions: Vec<NotificationAction>,
        resident: bool,
        timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        if self
            .notifications
            .get(&replaces)
            .is_some_and(|notification| notification.owner != owner)
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "Only the notifying sender can replace a notification".into(),
            ));
        }
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
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let timeout = if timeout < 0 { 5000 } else { timeout };
        self.notifications.insert(
            id,
            Notification {
                id,
                generation: self.next_generation,
                owner: owner.into(),
                actions,
                resident,
                app: app.into(),
                summary: summary.into(),
                body: body.into(),
                expires: (timeout > 0)
                    .then(|| Instant::now() + Duration::from_millis(timeout as u64)),
            },
        );
        Ok(id)
    }
    fn expired(&mut self, now: Instant) -> Vec<NotificationEvent> {
        let ids: Vec<_> = self
            .notifications
            .values()
            .filter(|notification| notification.expires.is_some_and(|expiry| expiry <= now))
            .map(|notification| notification.id)
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                self.notifications
                    .remove(&id)
                    .map(|notification| NotificationEvent::Closed {
                        id,
                        owner: notification.owner,
                        reason: 1,
                    })
            })
            .collect()
    }
    fn pending_events(&mut self, now: Instant) -> Vec<NotificationEvent> {
        // Enqueue already rejected expired cards. Honor accepted clicks before
        // applying the next expiry tick, so polling latency cannot swallow a
        // click made just before its deadline. Replacement generations still win.
        let mut events = Vec::new();
        while let Some(intent) = self.intents.pop_front() {
            let (id, generation) = match &intent {
                NotificationIntent::Action { id, generation, .. }
                | NotificationIntent::Dismiss { id, generation } => (*id, *generation),
            };
            let Some(notification) = self
                .notifications
                .get(&id)
                .filter(|notification| notification.generation == generation)
            else {
                continue;
            };
            let owner = notification.owner.clone();
            match intent {
                NotificationIntent::Action { key, .. } => {
                    if !notification.actions.iter().any(|action| action.key == key) {
                        continue;
                    }
                    let resident = notification.resident;
                    events.push(NotificationEvent::Action {
                        id,
                        owner: owner.clone(),
                        key,
                    });
                    if !resident {
                        self.notifications.remove(&id);
                        events.push(NotificationEvent::Closed {
                            id,
                            owner,
                            reason: 2,
                        });
                    }
                }
                NotificationIntent::Dismiss { .. } => {
                    self.notifications.remove(&id);
                    events.push(NotificationEvent::Closed {
                        id,
                        owner,
                        reason: 2,
                    });
                }
            }
        }
        events.extend(self.expired(now));
        events
    }
}

fn parse_actions(actions: Vec<String>) -> zbus::fdo::Result<Vec<NotificationAction>> {
    if !actions.len().is_multiple_of(2) || actions.len() > MAX_ACTIONS * 2 {
        return Err(zbus::fdo::Error::InvalidArgs(
            "Notification actions must contain at most eight key/label pairs".into(),
        ));
    }
    let mut keys = HashSet::new();
    let mut parsed = Vec::with_capacity(actions.len() / 2);
    let mut parts = actions.into_iter();
    while let Some(key) = parts.next() {
        let label = parts.next().expect("validated paired actions");
        if key.is_empty() || key.len() > 128 || label.len() > 512 || !keys.insert(key.clone()) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Notification action keys must be nonempty, unique and bounded".into(),
            ));
        }
        parsed.push(NotificationAction { key, label });
    }
    Ok(parsed)
}
fn sender(header: &Header<'_>) -> zbus::fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| zbus::fdo::Error::AccessDenied("A bus sender is required".into()))
}

/// Queue real UI input without D-Bus I/O. Stale and expired cards cannot invoke a replacement.
pub fn enqueue_action(state: &SharedState, id: u32, generation: u64, key: &str) -> bool {
    let mut state = state.lock().unwrap();
    let now = Instant::now();
    let valid = state.notifications.get(&id).is_some_and(|notification| {
        notification.generation == generation
            && notification.expires.is_none_or(|expiry| expiry > now)
            && notification.actions.iter().any(|action| action.key == key)
    });
    if !valid || state.intents.len() >= MAX_PENDING_INTENTS {
        return false;
    }
    state.intents.push_back(NotificationIntent::Action {
        id,
        generation,
        key: key.into(),
    });
    true
}
/// Queue an explicit user dismissal; the service emits reason 2 on its runtime.
pub fn enqueue_dismiss(state: &SharedState, id: u32, generation: u64) -> bool {
    let mut state = state.lock().unwrap();
    let now = Instant::now();
    let valid = state.notifications.get(&id).is_some_and(|notification| {
        notification.generation == generation
            && notification.expires.is_none_or(|expiry| expiry > now)
    });
    if !valid || state.intents.len() >= MAX_PENDING_INTENTS {
        return false;
    }
    state
        .intents
        .push_back(NotificationIntent::Dismiss { id, generation });
    true
}

async fn emit_notification_event(
    connection: &zbus::Connection,
    event: NotificationEvent,
) -> zbus::Result<()> {
    let owner = match &event {
        NotificationEvent::Action { owner, .. } | NotificationEvent::Closed { owner, .. } => owner,
    };
    let emitter = SignalEmitter::new(connection, NOTIFICATION_PATH)?
        .set_destination(zbus::names::BusName::try_from(owner.as_str())?);
    match &event {
        NotificationEvent::Action { id, key, .. } => {
            NotificationService::action_invoked(&emitter, *id, key).await
        }
        NotificationEvent::Closed { id, reason, .. } => {
            NotificationService::notification_closed(&emitter, *id, *reason).await
        }
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
        vec!["body", "actions", "persistence"]
    }
    fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("Aura Shell", "Aura DE", env!("CARGO_PKG_VERSION"), "1.2")
    }
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<u32> {
        let owner = sender(&header)?;
        let actions = parse_actions(actions)?;
        let resident = match hints.get("resident") {
            Some(value) => bool::try_from(value)
                .map_err(|_| zbus::fdo::Error::InvalidArgs("resident must be a boolean".into()))?,
            None => false,
        };
        let gate = self.state.lock().unwrap().notification_gate.clone();
        let _guard = gate.lock().await;
        self.state.lock().unwrap().notify(
            &owner,
            app_name,
            replaces_id,
            summary,
            body,
            actions,
            resident,
            expire_timeout,
        )
    }
    async fn close_notification(
        &self,
        id: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let owner = sender(&header)?;
        let gate = self.state.lock().unwrap().notification_gate.clone();
        let _guard = gate.lock().await;
        let removed = {
            let mut state = self.state.lock().unwrap();
            if state
                .notifications
                .get(&id)
                .is_some_and(|notification| notification.owner != owner)
            {
                return Err(zbus::fdo::Error::AccessDenied(
                    "Only the notifying sender can close a notification".into(),
                ));
            }
            state.notifications.remove(&id).is_some()
        };
        if removed {
            let emitter = emitter.set_destination(
                zbus::names::BusName::try_from(owner.as_str())
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            );
            Self::notification_closed(&emitter, id, 3)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        Ok(())
    }
    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
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
                let gate = state.lock().unwrap().notification_gate.clone();
                let _guard = gate.lock().await;
                let events = state.lock().unwrap().pending_events(Instant::now());
                for event in events {
                    if let Err(error) = emit_notification_event(&connection, event).await {
                        tracing::warn!(%error, "Notification signal delivery failed");
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
    fn notify(
        state: &mut ShellState,
        owner: &str,
        replaces: u32,
        actions: &[&str],
        resident: bool,
        timeout: i32,
    ) -> u32 {
        state
            .notify(
                owner,
                "app",
                replaces,
                "summary",
                "body",
                parse_actions(actions.iter().map(|value| (*value).into()).collect()).unwrap(),
                resident,
                timeout,
            )
            .unwrap()
    }
    #[test]
    fn replacement_ids_ownership_and_expiration() {
        let mut state = ShellState::default();
        let first = notify(&mut state, ":1.1", 0, &[], false, 100);
        let generation = state.notifications[&first].generation;
        assert!(
            state
                .notify(":1.2", "other", first, "foreign", "", vec![], false, 0)
                .is_err()
        );
        assert_eq!(first, notify(&mut state, ":1.1", first, &[], false, 0));
        assert_ne!(generation, state.notifications[&first].generation);
        assert!(
            state
                .expired(Instant::now() + Duration::from_secs(1))
                .is_empty()
        );
        let expiring = notify(&mut state, ":1.1", 0, &[], false, 1);
        assert_eq!(
            state.expired(Instant::now() + Duration::from_secs(1)),
            vec![NotificationEvent::Closed {
                id: expiring,
                owner: ":1.1".into(),
                reason: 1
            }]
        );
        assert!(state.notifications.contains_key(&first));
    }
    #[test]
    fn malformed_actions_are_rejected_without_silent_truncation() {
        for values in [
            vec!["open"],
            vec!["", "label"],
            vec!["open", "one", "open", "two"],
        ] {
            assert!(parse_actions(values.into_iter().map(String::from).collect()).is_err());
        }
        let many = (0..9)
            .flat_map(|index| [format!("key-{index}"), "label".into()])
            .collect();
        assert!(parse_actions(many).is_err());
        assert_eq!(
            parse_actions(vec!["default".into(), "".into()]).unwrap()[0].label,
            ""
        );
    }
    #[test]
    fn action_closes_nonresident_once_and_keeps_resident_until_dismissed() {
        let state = SharedState::default();
        let first = notify(
            &mut state.lock().unwrap(),
            ":1.1",
            0,
            &["open", "Open"],
            false,
            0,
        );
        let generation = state.lock().unwrap().notifications[&first].generation;
        assert!(!enqueue_action(&state, first, generation, "unknown"));
        assert!(enqueue_action(&state, first, generation, "open"));
        assert!(enqueue_action(&state, first, generation, "open"));
        assert_eq!(
            state.lock().unwrap().pending_events(Instant::now()),
            vec![
                NotificationEvent::Action {
                    id: first,
                    owner: ":1.1".into(),
                    key: "open".into()
                },
                NotificationEvent::Closed {
                    id: first,
                    owner: ":1.1".into(),
                    reason: 2
                },
            ]
        );
        let resident = notify(
            &mut state.lock().unwrap(),
            ":1.2",
            0,
            &["default", "Open"],
            true,
            0,
        );
        let generation = state.lock().unwrap().notifications[&resident].generation;
        assert!(enqueue_action(&state, resident, generation, "default"));
        assert_eq!(
            state.lock().unwrap().pending_events(Instant::now()),
            vec![NotificationEvent::Action {
                id: resident,
                owner: ":1.2".into(),
                key: "default".into()
            },]
        );
        assert!(state.lock().unwrap().notifications.contains_key(&resident));
        assert!(enqueue_dismiss(&state, resident, generation));
        assert_eq!(
            state.lock().unwrap().pending_events(Instant::now()),
            vec![NotificationEvent::Closed {
                id: resident,
                owner: ":1.2".into(),
                reason: 2
            },]
        );
    }
    #[test]
    fn rendered_generation_cannot_activate_or_dismiss_a_replacement() {
        let state = SharedState::default();
        let id = notify(
            &mut state.lock().unwrap(),
            ":1.1",
            0,
            &["open", "Open"],
            true,
            0,
        );
        let generation = state.lock().unwrap().notifications[&id].generation;
        assert!(enqueue_action(&state, id, generation, "open"));
        assert!(enqueue_dismiss(&state, id, generation));
        notify(
            &mut state.lock().unwrap(),
            ":1.1",
            id,
            &["open", "New open"],
            true,
            0,
        );
        assert!(!enqueue_action(&state, id, generation, "open"));
        assert!(!enqueue_dismiss(&state, id, generation));
        assert!(
            state
                .lock()
                .unwrap()
                .pending_events(Instant::now())
                .is_empty()
        );
        assert!(state.lock().unwrap().notifications.contains_key(&id));
    }
    #[test]
    fn accepted_click_is_not_lost_to_the_next_expiry_poll() {
        let state = SharedState::default();
        let id = notify(
            &mut state.lock().unwrap(),
            ":1.1",
            0,
            &["open", "Open"],
            false,
            1000,
        );
        let generation = state.lock().unwrap().notifications[&id].generation;
        assert!(enqueue_action(&state, id, generation, "open"));
        assert_eq!(
            state
                .lock()
                .unwrap()
                .pending_events(Instant::now() + Duration::from_secs(2)),
            vec![
                NotificationEvent::Action {
                    id,
                    owner: ":1.1".into(),
                    key: "open".into()
                },
                NotificationEvent::Closed {
                    id,
                    owner: ":1.1".into(),
                    reason: 2
                },
            ]
        );
    }
    #[test]
    fn expired_cards_reject_input_and_emit_only_expiry() {
        let state = SharedState::default();
        let id = notify(
            &mut state.lock().unwrap(),
            ":1.1",
            0,
            &["open", "Open"],
            false,
            1,
        );
        let generation = state.lock().unwrap().notifications[&id].generation;
        state
            .lock()
            .unwrap()
            .notifications
            .get_mut(&id)
            .unwrap()
            .expires = Some(Instant::now());
        assert!(!enqueue_action(&state, id, generation, "open"));
        assert!(!enqueue_dismiss(&state, id, generation));
        assert_eq!(
            state.lock().unwrap().pending_events(Instant::now()),
            vec![NotificationEvent::Closed {
                id,
                owner: ":1.1".into(),
                reason: 1
            },]
        );
    }
}
