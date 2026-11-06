//! Actual session settings with cancellable background transport and readback.
use crate::{
    client::{self, Canvas, DesktopUi, PanelSpec},
    ipc,
};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use std::{io, path::Path, sync::mpsc, thread, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc as async_mpsc, watch},
};

const DEADLINE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
struct CompositorSettings {
    layout: String,
    quality: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct SettingsState {
    compositor: CompositorSettings,
    do_not_disturb: bool,
    preferences_enabled: bool,
    persistence_error: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    Layout(&'static str),
    Quality(&'static str),
    DoNotDisturb(bool),
}
impl SettingsState {
    fn confirms(&self, change: &Change) -> bool {
        match change {
            Change::Layout(value) => self.compositor.layout == *value,
            Change::Quality(value) => self.compositor.quality == *value,
            Change::DoNotDisturb(value) => self.do_not_disturb == *value,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Update {
    state: Option<SettingsState>,
    error: Option<String>,
    completed: u64,
}
struct Request {
    id: u64,
    change: Change,
}

fn parse_status(status: &str) -> Result<CompositorSettings, String> {
    let state: CompositorSettings =
        serde_json::from_str(status).map_err(|error| error.to_string())?;
    if !matches!(
        state.layout.as_str(),
        "floating" | "stage-manager" | "tiling"
    ) || !matches!(
        state.quality.as_str(),
        "ultra" | "balanced" | "reduced-transparency"
    ) {
        return Err("The compositor returned unknown settings".into());
    }
    Ok(state)
}

async fn compositor_status(path: &Path) -> Result<CompositorSettings, String> {
    let mut stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"status\n")
        .await
        .map_err(|error| error.to_string())?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(64 * 1024)
        .read_line(&mut response)
        .await
        .map_err(|error| error.to_string())?;
    if !response.ends_with('\n') || response.starts_with("ERR") {
        return Err("The compositor did not return a complete settings response".into());
    }
    parse_status(&response)
}

async fn transact(change: Option<&Change>) -> Result<SettingsState, String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&connection)
        .destination("org.aura.Shell1")
        .map_err(|error| error.to_string())?
        .path("/org/aura/Shell1")
        .map_err(|error| error.to_string())?
        .interface("org.aura.Shell1")
        .map_err(|error| error.to_string())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|error| error.to_string())?;
    if let Some(change) = change {
        let acknowledgement = match change {
            Change::Layout(value) => {
                Some(proxy.call::<_, _, String>("SetLayoutMode", &(value,)).await)
            }
            Change::Quality(value) => Some(
                proxy
                    .call::<_, _, String>("SetGlassQuality", &(value,))
                    .await,
            ),
            Change::DoNotDisturb(value) => {
                proxy
                    .set_property("DoNotDisturb", value)
                    .await
                    .map_err(|error| error.to_string())?;
                None
            }
        };
        if let Some(acknowledgement) = acknowledgement
            && acknowledgement.map_err(|error| error.to_string())? != "OK"
        {
            return Err("The shell did not acknowledge the settings change".into());
        }
    }
    let state = SettingsState {
        compositor: compositor_status(&ipc::control_path()?).await?,
        do_not_disturb: proxy
            .get_property("DoNotDisturb")
            .await
            .map_err(|error| error.to_string())?,
        preferences_enabled: proxy
            .get_property("PreferencesEnabled")
            .await
            .map_err(|error| error.to_string())?,
        persistence_error: proxy
            .get_property("PersistenceError")
            .await
            .map_err(|error| error.to_string())?,
    };
    if change.is_some_and(|change| !state.confirms(change)) {
        return Err("The requested setting was not confirmed by readback".into());
    }
    Ok(state)
}

async fn refresh(
    mut requests: async_mpsc::Receiver<Request>,
    publish: mpsc::SyncSender<Update>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut timer = tokio::time::interval(POLL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut current = Update::default();
    let mut published = None;
    let mut action_error = None;
    loop {
        let request = tokio::select! {
            _ = stopped.changed() => break,
            request = requests.recv() => match request { Some(request) => Some(request), None => break },
            _ = timer.tick() => None,
        };
        let result = tokio::select! {
            _ = stopped.changed() => break,
            result = tokio::time::timeout(DEADLINE, transact(request.as_ref().map(|request| &request.change))) => {
                result.unwrap_or_else(|_| Err("The desktop settings request timed out".into()))
            }
        };
        let requested = request.is_some();
        if let Some(request) = request {
            current.completed = request.id;
        }
        match result {
            Ok(state) => {
                let persistence_error =
                    (!state.persistence_error.is_empty()).then(|| state.persistence_error.clone());
                current.state = Some(state);
                if requested {
                    action_error = None;
                }
                current.error = persistence_error.or_else(|| action_error.clone());
            }
            Err(error) => {
                if requested {
                    action_error = Some(error.clone());
                }
                current.error = Some(error);
            }
        }
        if published.as_ref() != Some(&current) {
            match publish.try_send(current.clone()) {
                Ok(()) => published = Some(current.clone()),
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    }
}

struct SettingsUi {
    requests: async_mpsc::Sender<Request>,
    updates: mpsc::Receiver<Update>,
    stop: watch::Sender<bool>,
    worker: Option<thread::JoinHandle<()>>,
    current: Update,
    pending: Option<(u64, Change)>,
    next_id: u64,
    selected: usize,
}
impl SettingsUi {
    fn new() -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (requests, receive) = async_mpsc::channel(8);
        let (publish, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = watch::channel(false);
        let worker = thread::Builder::new()
            .name("aura-settings".into())
            .spawn(move || runtime.block_on(refresh(receive, publish, stopped)))?;
        Ok(Self {
            requests,
            updates,
            stop,
            worker: Some(worker),
            current: Update::default(),
            pending: None,
            next_id: 0,
            selected: 0,
        })
    }
    fn change(&self, index: usize) -> Option<Change> {
        match index {
            0 => Some(Change::Layout("floating")),
            1 => Some(Change::Layout("stage-manager")),
            2 => Some(Change::Layout("tiling")),
            3 => Some(Change::Quality("ultra")),
            4 => Some(Change::Quality("balanced")),
            5 => Some(Change::Quality("reduced-transparency")),
            6 => match self.pending.as_ref().map(|(_, change)| change) {
                Some(Change::DoNotDisturb(value)) => Some(Change::DoNotDisturb(!value)),
                _ => self
                    .current
                    .state
                    .as_ref()
                    .map(|state| Change::DoNotDisturb(!state.do_not_disturb)),
            },
            _ => None,
        }
    }
    fn apply(&mut self) {
        if let Some(change) = self.change(self.selected) {
            self.next_id += 1;
            match self.requests.try_send(Request {
                id: self.next_id,
                change: change.clone(),
            }) {
                Ok(()) => self.pending = Some((self.next_id, change)),
                Err(error) => {
                    self.current.error = Some(format!("Settings worker unavailable: {error}"))
                }
            }
        }
    }
}
impl Drop for SettingsUi {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn bounds(index: usize) -> (i32, i32, u32, u32) {
    if index == 6 {
        (24, 238, 220, 36)
    } else {
        (
            24 + (index % 3) as i32 * 190,
            if index < 3 { 86 } else { 162 },
            172,
            36,
        )
    }
}
impl DesktopUi for SettingsUi {
    fn needs_redraw(&mut self) -> bool {
        match self.updates.try_recv() {
            Ok(update) => {
                let changed = self.current != update;
                self.current = update;
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|(id, _)| self.current.completed >= *id)
                {
                    self.pending = None;
                }
                changed
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                let error = "Settings worker disconnected".to_string();
                let changed = self.current.error.as_ref() != Some(&error);
                self.current.error = Some(error);
                self.pending = None;
                changed
            }
        }
    }
    fn tick_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(100))
    }
    fn draw(&mut self, _: usize, canvas: &mut Canvas<'_>) {
        canvas.clear(0xff192534);
        canvas.text(24, 20, "Desktop settings", 0xfff0f5fc, 2);
        let persistent = self
            .current
            .state
            .as_ref()
            .is_some_and(|state| state.preferences_enabled);
        let scope = if persistent {
            "Changes here are saved between Aura sessions"
        } else {
            "Applies to the current Aura session"
        };
        canvas.text(24, 50, scope, 0xffb8c8dc, 1);
        canvas.text(24, 68, "Window layout", 0xffd9e8f8, 1);
        canvas.text(24, 144, "Glass quality", 0xffd9e8f8, 1);
        canvas.text(24, 220, "Notifications", 0xffd9e8f8, 1);
        let dnd = self
            .current
            .state
            .as_ref()
            .is_some_and(|state| state.do_not_disturb);
        for (index, label) in [
            "Floating",
            "Stage Manager",
            "Tiling",
            "Ultra",
            "Balanced",
            "Reduced transparency",
            if dnd {
                "Do not disturb: On"
            } else {
                "Do not disturb: Off"
            },
        ]
        .iter()
        .enumerate()
        {
            let (x, y, width, height) = bounds(index);
            let applied = self.change(index).is_some_and(|change| {
                self.current
                    .state
                    .as_ref()
                    .is_some_and(|state| match change {
                        Change::DoNotDisturb(_) => state.do_not_disturb,
                        _ => state.confirms(&change),
                    })
            });
            canvas.rect(
                x - 2,
                y - 2,
                width + 4,
                height + 4,
                if self.selected == index {
                    0xff9bcaff
                } else {
                    0xff192534
                },
            );
            canvas.rect(
                x,
                y,
                width,
                height,
                if applied { 0xff365d84 } else { 0xff2a3c51 },
            );
            canvas.text(x + 8, y + 14, label, 0xffedf3fc, 1);
        }
        let message = if self.pending.is_some() {
            if persistent {
                "Applying and saving settings..."
            } else {
                "Applying settings..."
            }
        } else if let Some(error) = &self.current.error {
            error
        } else if self.current.state.is_none() {
            "Loading current settings..."
        } else {
            "Settings reflect the running desktop."
        };
        let text: Vec<_> = message
            .chars()
            .filter(|c| !c.is_control())
            .take(136)
            .collect();
        for (line, chunk) in text.chunks(68).take(2).enumerate() {
            canvas.text(
                24,
                294 + line as i32 * 14,
                &chunk.iter().collect::<String>(),
                if self.current.error.is_some() {
                    0xffffb2b2
                } else {
                    0xffb8c8dc
                },
                1,
            );
        }
        canvas.text(
            24,
            338,
            "Tab / arrows: select | Enter: apply | Esc: close",
            0xffa4bbd3,
            1,
        );
    }
    fn key(&mut self, key: u32, _: Option<&str>) -> bool {
        match key {
            0xff1b => return true,
            0xff09 | 0xff53 | 0xff54 => self.selected = (self.selected + 1) % 7,
            0xfe20 | 0xff51 | 0xff52 => self.selected = (self.selected + 6) % 7,
            0xff0d | 0xff8d => self.apply(),
            _ => {}
        }
        false
    }
    fn pointer(&mut self, _: usize, x: f64, y: f64, pressed: bool) {
        if pressed
            && let Some(index) = (0..7).find(|index| {
                let (left, top, width, height) = bounds(*index);
                x >= f64::from(left)
                    && x < f64::from(left) + f64::from(width)
                    && y >= f64::from(top)
                    && y < f64::from(top) + f64::from(height)
            })
        {
            self.selected = index;
            self.apply();
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    client::run(
        vec![PanelSpec {
            namespace: "aura-settings",
            width: 600,
            height: 360,
            anchor: Anchor::empty(),
            layer: Layer::Overlay,
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::Exclusive,
        }],
        Box::new(SettingsUi::new()?),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readback_rejects_missing_unknown_or_error_settings() {
        assert!(parse_status("ERR unavailable\n").is_err());
        assert!(parse_status(r#"{"layout":"tiling"}"#).is_err());
        assert!(parse_status(r#"{"layout":"unknown","quality":"balanced"}"#).is_err());
        let compositor =
            parse_status(r#"{"layout":"tiling","quality":"balanced","extra":true}"#).unwrap();
        let state = SettingsState {
            compositor,
            do_not_disturb: false,
            preferences_enabled: true,
            persistence_error: String::new(),
        };
        assert!(state.confirms(&Change::Layout("tiling")));
        assert!(!state.confirms(&Change::Layout("floating")));
        assert!(!state.confirms(&Change::Quality("ultra")));
        assert!(!state.confirms(&Change::DoNotDisturb(true)));
    }
    #[test]
    fn control_targets_remain_distinct_and_inside_the_panel() {
        for index in 0..7 {
            let (x, y, width, height) = bounds(index);
            assert!(x >= 0 && y >= 0 && x + width as i32 <= 600 && y + height as i32 <= 360);
            for other in index + 1..7 {
                let (ox, oy, ow, oh) = bounds(other);
                assert!(
                    x + width as i32 <= ox
                        || ox + ow as i32 <= x
                        || y + height as i32 <= oy
                        || oy + oh as i32 <= y
                );
            }
        }
    }
}
