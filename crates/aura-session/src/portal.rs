//! ScreenCast backend with explicit user consent and real PipeWire stream ownership.
//! The standalone service advertises no sources; the session supervisor supplies
//! an in-memory capability only after its own compositor has started.

use crate::capture::{CaptureConfig, CaptureStream};
use futures_lite::StreamExt;
use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    task::JoinHandle,
};
use zbus::{
    Connection, fdo,
    message::Header,
    object_server::{ObjectServer, SignalEmitter},
    zvariant::{ObjectPath, OwnedValue, Value},
};

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.aura";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const FRONTEND_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request/";
const SESSION_PREFIX: &str = "/org/freedesktop/portal/desktop/session/";
const MAX_SESSIONS: usize = 256;
const MAX_STREAMS: usize = 4;
const CAPTURE_UNAVAILABLE: &str = "Aura ScreenCast requires its authenticated compositor capture source and a running PipeWire daemon";
type Options = HashMap<String, OwnedValue>;
type Response = (u32, Options);
type Sessions = Arc<Mutex<HashMap<String, SessionRecord>>>;
type Requests = Arc<Mutex<HashMap<String, RequestRecord>>>;

/// Arguments are passed directly to this trusted UI executable, never to a shell.
#[derive(Clone, Debug)]
pub struct ConsentUi {
    pub program: OsString,
    pub wayland_display: OsString,
    pub runtime_dir: PathBuf,
}

struct SessionRecord {
    owner: String,
    app_id: String,
    selected: bool,
    starting: Option<Arc<()>>,
    closed: Arc<AtomicBool>,
    capture: Option<CaptureStream>,
}
impl Drop for SessionRecord {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(stream) = &self.capture {
            stream.stop();
        }
    }
}

/// Clear only this Start attempt's claim. A rejected concurrent Start must not
/// clear the first request's in-progress flag and allow a second consent flow.
struct StartingClaim {
    sessions: Sessions,
    path: String,
    token: Arc<()>,
}
impl Drop for StartingClaim {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.sessions.lock()
            && let Some(session) = sessions.get_mut(&self.path)
            && session
                .starting
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            session.starting = None;
        }
    }
}

/// Reserve the registry entry before publishing Session.Close. Failed creation
/// rolls back only this generation, so a closed path can be reused safely.
struct SessionRegistration {
    sessions: Sessions,
    path: String,
    closed: Arc<AtomicBool>,
    committed: bool,
}
impl SessionRegistration {
    fn reserve(sessions: Sessions, path: &str, owner: String, app_id: String) -> fdo::Result<Self> {
        let closed = Arc::new(AtomicBool::new(false));
        {
            let mut registry = sessions.lock().unwrap();
            if registry.len() >= MAX_SESSIONS {
                return Err(fdo::Error::LimitsExceeded(
                    "Too many active portal sessions".into(),
                ));
            }
            if registry.contains_key(path) {
                return Err(fdo::Error::InvalidArgs(
                    "Portal session handle is already in use".into(),
                ));
            }
            registry.insert(
                path.into(),
                SessionRecord {
                    owner,
                    app_id,
                    selected: false,
                    starting: None,
                    closed: closed.clone(),
                    capture: None,
                },
            );
        }
        Ok(Self {
            sessions,
            path: path.into(),
            closed,
            committed: false,
        })
    }

    fn discard(&self) -> bool {
        remove_session(&self.sessions, &self.path, &self.closed)
    }
}
impl Drop for SessionRegistration {
    fn drop(&mut self) {
        if !self.committed {
            self.discard();
        }
    }
}

fn remove_session(sessions: &Sessions, path: &str, closed: &Arc<AtomicBool>) -> bool {
    let mut registry = sessions.lock().unwrap();
    if registry
        .get(path)
        .is_some_and(|record| Arc::ptr_eq(&record.closed, closed))
    {
        registry.remove(path);
        true
    } else {
        false
    }
}

struct RequestRecord {
    owner: String,
    cancelled: Arc<AtomicBool>,
}

pub struct PortalService {
    _connection: Connection,
    sessions: Sessions,
    requests: Requests,
    cleanup: JoinHandle<()>,
}
impl Drop for PortalService {
    fn drop(&mut self) {
        self.cleanup.abort();
        self.sessions.lock().unwrap().clear();
        let mut requests = self.requests.lock().unwrap();
        for request in requests.values() {
            request.cancelled.store(true, Ordering::Release);
        }
        requests.clear();
    }
}
#[derive(Default)]
struct ScreenCast {
    sessions: Sessions,
    requests: Requests,
    capture: Option<CaptureConfig>,
    consent: Option<ConsentUi>,
}
fn sender(header: &Header<'_>) -> fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| fdo::Error::AccessDenied("A bus sender is required".into()))
}
async fn frontend_sender(header: &Header<'_>, connection: &Connection) -> fdo::Result<String> {
    let sender = sender(header)?;
    // Only the standard portal frontend can vouch for app_id. An arbitrary
    // native client could otherwise display consent under another app's name.
    // Resolve on every call so a frontend restart cannot reuse stale authority.
    let bus = fdo::DBusProxy::new(connection)
        .await
        .map_err(|error| fdo::Error::Failed(error.to_string()))?;
    let frontend = bus
        .get_name_owner(FRONTEND_BUS_NAME.try_into().unwrap())
        .await
        .map_err(|_| {
            fdo::Error::AccessDenied("The standard portal frontend must own its bus name".into())
        })?;
    if frontend.as_str() != sender {
        return Err(fdo::Error::AccessDenied(
            "ScreenCast requests must come from the standard portal frontend".into(),
        ));
    }
    Ok(sender)
}
fn require_path(path: &ObjectPath<'_>, prefix: &str) -> fdo::Result<()> {
    if path.as_str().starts_with(prefix) && path.as_str().len() > prefix.len() {
        Ok(())
    } else {
        Err(fdo::Error::InvalidArgs(format!(
            "Expected an object path below {prefix}"
        )))
    }
}
fn authorize_record<'a>(
    registry: &'a mut HashMap<String, SessionRecord>,
    path: &ObjectPath<'_>,
    app_id: &str,
    owner: &str,
) -> fdo::Result<&'a mut SessionRecord> {
    let session = registry
        .get_mut(path.as_str())
        .ok_or_else(|| fdo::Error::InvalidArgs("Unknown or closed portal session".into()))?;
    if session.owner != owner || session.app_id != app_id {
        return Err(fdo::Error::AccessDenied(
            "Portal session belongs to another sender or application".into(),
        ));
    }
    Ok(session)
}
async fn authorize(
    sessions: &Sessions,
    path: &ObjectPath<'_>,
    app_id: &str,
    owner: &str,
) -> fdo::Result<()> {
    authorize_record(&mut sessions.lock().unwrap(), path, app_id, owner).map(|_| ())
}
struct PortalRequest {
    path: String,
    owner: String,
    cancelled: Arc<AtomicBool>,
    requests: Requests,
}
#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl PortalRequest {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<()> {
        if sender(&header)? != self.owner {
            return Err(fdo::Error::AccessDenied(
                "Only the creating portal frontend may cancel this request".into(),
            ));
        }
        self.cancelled.store(true, Ordering::Release);
        finish_request(server, &self.requests, &self.path, &self.cancelled).await;
        Ok(())
    }
}
async fn begin_request(
    server: &ObjectServer,
    path: &ObjectPath<'_>,
    requests: &Requests,
    owner: &str,
) -> fdo::Result<Arc<AtomicBool>> {
    require_path(path, REQUEST_PREFIX)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut requests = requests.lock().unwrap();
        if requests.contains_key(path.as_str()) {
            return Err(fdo::Error::InvalidArgs(
                "Portal request handle is already in use".into(),
            ));
        }
        requests.insert(
            path.to_string(),
            RequestRecord {
                owner: owner.into(),
                cancelled: cancelled.clone(),
            },
        );
    }
    let result = server
        .at(
            path.clone(),
            PortalRequest {
                path: path.to_string(),
                owner: owner.into(),
                cancelled: cancelled.clone(),
                requests: requests.clone(),
            },
        )
        .await;
    if matches!(result, Ok(true)) {
        return Ok(cancelled);
    }
    remove_request(requests, path.as_str(), &cancelled);
    match result {
        Err(error) => Err(fdo::Error::Failed(error.to_string())),
        Ok(_) => Err(fdo::Error::InvalidArgs(
            "Portal request handle is already in use".into(),
        )),
    }
}
fn remove_request(requests: &Requests, path: &str, cancelled: &Arc<AtomicBool>) -> bool {
    let mut registry = requests.lock().unwrap();
    if registry
        .get(path)
        .is_some_and(|record| Arc::ptr_eq(&record.cancelled, cancelled))
    {
        registry.remove(path);
        true
    } else {
        false
    }
}
async fn finish_request(
    server: &ObjectServer,
    requests: &Requests,
    path: &str,
    cancelled: &Arc<AtomicBool>,
) {
    if remove_request(requests, path, cancelled) {
        let _ = server.remove::<PortalRequest, _>(path).await;
    }
}
struct PortalSession {
    path: String,
    owner: String,
    closed: Arc<AtomicBool>,
    sessions: Sessions,
}
#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl PortalSession {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<()> {
        if sender(&header)? != self.owner {
            return Err(fdo::Error::AccessDenied(
                "Only the creating portal frontend may close this session".into(),
            ));
        }
        if remove_session(&self.sessions, &self.path, &self.closed) {
            server
                .remove::<PortalSession, _>(self.path.as_str())
                .await
                .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        }
        Ok(())
    }
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

fn selection_options(options: &Options) -> fdo::Result<()> {
    let number = |name: &str, default: u32| -> fdo::Result<u32> {
        options
            .get(name)
            .map(|v| {
                u32::try_from(v).map_err(|_| {
                    fdo::Error::InvalidArgs(format!("{name} must be an unsigned integer"))
                })
            })
            .unwrap_or(Ok(default))
    };
    if number("types", 1)? != 1 {
        return Err(fdo::Error::InvalidArgs(
            "Only a single monitor source is supported".into(),
        ));
    }
    if number("cursor_mode", 1)? != 1 {
        return Err(fdo::Error::InvalidArgs(
            "Only Hidden cursor mode is currently supported".into(),
        ));
    }
    if number("persist_mode", 0)? != 0 || options.contains_key("restore_token") {
        return Err(fdo::Error::InvalidArgs(
            "Persistent capture permissions are not supported".into(),
        ));
    }
    if let Some(value) = options.get("multiple")
        && bool::try_from(value)
            .map_err(|_| fdo::Error::InvalidArgs("multiple must be a boolean".into()))?
    {
        return Err(fdo::Error::InvalidArgs(
            "Multiple capture sources are not supported".into(),
        ));
    }
    Ok(())
}
#[derive(Debug, PartialEq, Eq)]
enum ConsentOutcome {
    Approved,
    Denied,
    Cancelled,
}
async fn wait_cancelled(request: &AtomicBool, session: &AtomicBool) {
    loop {
        if request.load(Ordering::Acquire) || session.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
async fn wait_for_consent(
    child: &mut Child,
    request: &AtomicBool,
    session: &AtomicBool,
) -> fdo::Result<ConsentOutcome> {
    let outcome = tokio::select! {
        status=child.wait()=>{
            let status=status.map_err(|e|fdo::Error::Failed(format!("Consent UI failed: {e}")))?;
            if !status.success(){return Err(fdo::Error::Failed("Consent UI could not complete".into()));}
            let mut output=Vec::new();
            if let Some(stdout)=child.stdout.take(){stdout.take(128).read_to_end(&mut output).await.map_err(|e|fdo::Error::Failed(format!("Consent response failed: {e}")))?;}
            if output==b"approved\n"{ConsentOutcome::Approved}else{ConsentOutcome::Denied}
        },
        ()=wait_cancelled(request,session)=>ConsentOutcome::Cancelled,
        ()=tokio::time::sleep(Duration::from_secs(120))=>ConsentOutcome::Denied,
    };
    if outcome != ConsentOutcome::Approved && child.try_wait().ok().flatten().is_none() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    if request.load(Ordering::Acquire) || session.load(Ordering::Acquire) {
        return Ok(ConsentOutcome::Cancelled);
    }
    Ok(outcome)
}
async fn prompt_consent(
    ui: &ConsentUi,
    app_id: &str,
    request: &AtomicBool,
    session: &AtomicBool,
) -> fdo::Result<ConsentOutcome> {
    if request.load(Ordering::Acquire) || session.load(Ordering::Acquire) {
        return Ok(ConsentOutcome::Cancelled);
    }
    let mut child = Command::new(&ui.program)
        .args(["--screen-cast-consent", app_id])
        .env("WAYLAND_DISPLAY", &ui.wayland_display)
        .env("XDG_RUNTIME_DIR", &ui.runtime_dir)
        .env_remove("AURA_CAPTURE_TOKEN")
        .env_remove("AURA_CAPTURE_SOCKET")
        .env_remove("AURA_CAPTURE_OWNER_PID")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            fdo::Error::Failed(format!("Could not open screen-sharing consent UI: {e}"))
        })?;
    wait_for_consent(&mut child, request, session).await
}

impl ScreenCast {
    async fn start_capture(
        &self,
        path: &ObjectPath<'_>,
        app_id: &str,
        owner: &str,
        cancelled: Arc<AtomicBool>,
    ) -> fdo::Result<Response> {
        let config = self
            .capture
            .clone()
            .ok_or_else(|| fdo::Error::NotSupported(CAPTURE_UNAVAILABLE.into()))?;
        let ui = self.consent.as_ref().ok_or_else(|| {
            fdo::Error::NotSupported("Screen-sharing consent UI is unavailable".into())
        })?;
        let (closed, _claim) = {
            let mut sessions = self.sessions.lock().unwrap();
            let active = sessions
                .values()
                .filter(|s| s.capture.is_some() || s.starting.is_some())
                .count();
            if active >= MAX_STREAMS {
                return Err(fdo::Error::LimitsExceeded(
                    "Too many simultaneous screen-sharing streams".into(),
                ));
            }
            let session = authorize_record(&mut sessions, path, app_id, owner)?;
            if !session.selected {
                return Err(fdo::Error::InvalidArgs(
                    "SelectSources must complete before Start".into(),
                ));
            }
            if session.starting.is_some() || session.capture.is_some() {
                return Err(fdo::Error::InvalidArgs(
                    "The portal session has already started".into(),
                ));
            }
            let token = Arc::new(());
            session.starting = Some(token.clone());
            (
                session.closed.clone(),
                StartingClaim {
                    sessions: self.sessions.clone(),
                    path: path.to_string(),
                    token,
                },
            )
        };
        match prompt_consent(ui, app_id, &cancelled, &closed).await? {
            ConsentOutcome::Approved => {}
            ConsentOutcome::Denied | ConsentOutcome::Cancelled => return Ok((1, Options::new())),
        }
        // Keep Request.Close connected to the live worker even if cancellation
        // arrives between the final check and committing the session stream.
        // Normal request completion removes its object without setting this.
        let stop = cancelled.clone();
        let stop_worker = stop.clone();
        let mut startup = tokio::task::spawn_blocking(move || {
            CaptureStream::start_with_cancel(config, stop_worker)
        });
        let stream = tokio::select! {
            result=&mut startup=>result,
            ()=wait_cancelled(&cancelled,&closed)=>{
                stop.store(true,Ordering::Release);
                startup.await
            }
        }
        .map_err(|error| fdo::Error::Failed(format!("Screen capture worker failed: {error}")))?;
        let stream = match stream {
            Ok(stream) => stream,
            Err(error)
                if closed.load(Ordering::Acquire)
                    || (cancelled.load(Ordering::Acquire)
                        && error.kind() == std::io::ErrorKind::Interrupted) =>
            {
                return Ok((1, Options::new()));
            }
            Err(error) => {
                return Err(fdo::Error::Failed(format!(
                    "Screen capture could not start: {error}"
                )));
            }
        };
        if cancelled.load(Ordering::Acquire) || closed.load(Ordering::Acquire) {
            stream.stop();
            return Ok((1, Options::new()));
        }
        let properties = Options::from([
            (
                "size".into(),
                OwnedValue::try_from(Value::from((stream.width as i32, stream.height as i32)))
                    .map_err(|e| fdo::Error::Failed(e.to_string()))?,
            ),
            (
                "position".into(),
                OwnedValue::try_from(Value::from((0_i32, 0_i32)))
                    .map_err(|e| fdo::Error::Failed(e.to_string()))?,
            ),
            ("source_type".into(), OwnedValue::from(1_u32)),
        ]);
        let streams = OwnedValue::try_from(Value::from(vec![(stream.node_id, properties)]))
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        {
            let mut sessions = self.sessions.lock().unwrap();
            let session = authorize_record(&mut sessions, path, app_id, owner)?;
            if cancelled.load(Ordering::Acquire) || session.closed.load(Ordering::Acquire) {
                stream.stop();
                return Ok((1, Options::new()));
            }
            session.capture = Some(stream);
        }
        Ok((0, Options::from([("streams".into(), streams)])))
    }
}
#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    #[allow(clippy::too_many_arguments)] // Portal ABI plus injected D-Bus context.
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<Response> {
        let owner = frontend_sender(&header, connection).await?;
        require_path(&handle, REQUEST_PREFIX)?;
        require_path(&session_handle, SESSION_PREFIX)?;
        let mut registration = SessionRegistration::reserve(
            self.sessions.clone(),
            session_handle.as_str(),
            owner.clone(),
            app_id.into(),
        )?;
        let cancelled = begin_request(server, &handle, &self.requests, &owner).await?;
        let result = server
            .at(
                session_handle.clone(),
                PortalSession {
                    path: session_handle.to_string(),
                    owner: owner.clone(),
                    closed: registration.closed.clone(),
                    sessions: self.sessions.clone(),
                },
            )
            .await;
        finish_request(server, &self.requests, handle.as_str(), &cancelled).await;
        let added = result.map_err(|e| fdo::Error::Failed(e.to_string()))?;
        if !added {
            return Err(fdo::Error::InvalidArgs(
                "Portal session path is already registered".into(),
            ));
        }
        if cancelled.load(Ordering::Acquire) || registration.closed.load(Ordering::Acquire) {
            if registration.discard() {
                let _ = server
                    .remove::<PortalSession, _>(session_handle.clone())
                    .await;
            }
            return Ok((1, Options::new()));
        }
        registration.committed = true;
        Ok((0, Options::new()))
    }
    #[allow(clippy::too_many_arguments)] // Portal ABI plus injected D-Bus context.
    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<Response> {
        let owner = frontend_sender(&header, connection).await?;
        require_path(&handle, REQUEST_PREFIX)?;
        authorize(&self.sessions, &session_handle, app_id, &owner).await?;
        if self.capture.is_none() {
            return Err(fdo::Error::NotSupported(CAPTURE_UNAVAILABLE.into()));
        }
        selection_options(&options)?;
        let cancelled = begin_request(server, &handle, &self.requests, &owner).await?;
        let result = (|| {
            let mut sessions = self.sessions.lock().unwrap();
            let session = authorize_record(&mut sessions, &session_handle, app_id, &owner)?;
            if session.starting.is_some() || session.capture.is_some() {
                Err(fdo::Error::InvalidArgs(
                    "Cannot replace a running capture source".into(),
                ))
            } else {
                session.selected = true;
                Ok((0, Options::new()))
            }
        })();
        finish_request(server, &self.requests, handle.as_str(), &cancelled).await;
        if cancelled.load(Ordering::Acquire) {
            return Ok((1, Options::new()));
        }
        result
    }
    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<Response> {
        let owner = frontend_sender(&header, connection).await?;
        require_path(&handle, REQUEST_PREFIX)?;
        authorize(&self.sessions, &session_handle, app_id, &owner).await?;
        if self.capture.is_none() {
            return Err(fdo::Error::NotSupported(CAPTURE_UNAVAILABLE.into()));
        }
        let cancelled = begin_request(server, &handle, &self.requests, &owner).await?;
        let result = self
            .start_capture(&session_handle, app_id, &owner, cancelled.clone())
            .await;
        finish_request(server, &self.requests, handle.as_str(), &cancelled).await;
        result
    }
    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        u32::from(self.capture.is_some())
    }
    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        u32::from(self.capture.is_some())
    }
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

pub async fn serve() -> zbus::Result<PortalService> {
    serve_backend(ScreenCast::default()).await
}
/// Advertise capture only after authenticating the compositor's no-frame probe
/// and confirming a real PipeWire daemon can be reached.
pub async fn serve_with_capture(
    config: CaptureConfig,
    consent: ConsentUi,
) -> zbus::Result<PortalService> {
    // The Wayland socket is published before GPU/capture initialization. Give
    // our compositor a bounded opportunity to finish initialization, rather
    // than permanently advertising zero sources after this startup race.
    let available = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let candidate = config.clone();
            if matches!(
                tokio::task::spawn_blocking(move || crate::capture::probe(&candidate)).await,
                Ok(Ok(()))
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok();
    let backend = if available {
        ScreenCast {
            capture: Some(config),
            consent: Some(consent),
            ..Default::default()
        }
    } else {
        tracing::warn!(
            "Screen capture source or PipeWire is not ready; advertising zero capture capabilities"
        );
        ScreenCast::default()
    };
    serve_backend(backend).await
}
async fn serve_backend(backend: ScreenCast) -> zbus::Result<PortalService> {
    let sessions = backend.sessions.clone();
    let requests = backend.requests.clone();
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, backend)?
        .build()
        .await?;
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;
    let cleanup_connection = connection.clone();
    let cleanup_sessions = sessions.clone();
    let cleanup_requests = requests.clone();
    let cleanup = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            let dead_owner = tokio::select! {
                change=owner_changes.next()=>{
                    let Some(change)=change else {break};
                    let Ok(args)=change.args() else {continue};
                    if args.new_owner().is_some()||!args.name().as_str().starts_with(':'){continue;}
                    Some(args.name().to_string())
                },
                _=tick.tick()=>None,
            };
            let removed = {
                let mut sessions = cleanup_sessions.lock().unwrap();
                let mut removed = Vec::new();
                sessions.retain(|path, record| {
                    let dead = dead_owner
                        .as_ref()
                        .is_some_and(|owner| &record.owner == owner)
                        || record
                            .capture
                            .as_ref()
                            .is_some_and(|stream| !stream.is_running());
                    if dead {
                        removed.push(path.clone());
                        false
                    } else {
                        true
                    }
                });
                removed
            };
            let cancelled_requests = {
                let mut requests = cleanup_requests.lock().unwrap();
                let mut removed = Vec::new();
                requests.retain(|path, record| {
                    if dead_owner
                        .as_ref()
                        .is_some_and(|owner| &record.owner == owner)
                    {
                        record.cancelled.store(true, Ordering::Release);
                        removed.push(path.clone());
                        false
                    } else {
                        true
                    }
                });
                removed
            };
            for path in cancelled_requests {
                let _ = cleanup_connection
                    .object_server()
                    .remove::<PortalRequest, _>(path)
                    .await;
            }
            for path in removed {
                if let Ok(emitter) = SignalEmitter::new(&cleanup_connection, path.as_str()) {
                    let _ = PortalSession::closed(&emitter).await;
                }
                let _ = cleanup_connection
                    .object_server()
                    .remove::<PortalSession, _>(path)
                    .await;
            }
        }
    });
    tracing::info!(
        bus_name = BUS_NAME,
        "ScreenCast portal registered with explicit capture capabilities"
    );
    Ok(PortalService {
        _connection: connection,
        sessions,
        requests,
        cleanup,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    static TEST_BUS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn monitor_selection_rejects_unsupported_permissions_and_types() {
        assert!(selection_options(&Options::new()).is_ok());
        for options in [
            Options::from([("types".into(), OwnedValue::from(2_u32))]),
            Options::from([("types".into(), OwnedValue::from(true))]),
            Options::from([("cursor_mode".into(), OwnedValue::from(2_u32))]),
            Options::from([("multiple".into(), OwnedValue::from(true))]),
            Options::from([("multiple".into(), OwnedValue::from(1_u32))]),
            Options::from([("persist_mode".into(), OwnedValue::from(1_u32))]),
            Options::from([("restore_token".into(), OwnedValue::from(1_u32))]),
        ] {
            assert!(selection_options(&options).is_err());
        }
        assert!(
            selection_options(&Options::from([
                ("types".into(), OwnedValue::from(1_u32)),
                ("cursor_mode".into(), OwnedValue::from(1_u32)),
                ("multiple".into(), OwnedValue::from(false)),
            ]))
            .is_ok()
        );
    }

    #[tokio::test]
    async fn consent_requires_exact_approval_and_cancellation_reaps_ui() {
        let request = AtomicBool::new(false);
        let session = AtomicBool::new(false);
        for (script, expected) in [
            ("printf 'approved\\n'", ConsentOutcome::Approved),
            ("printf 'denied\\n'", ConsentOutcome::Denied),
            ("printf 'approved'", ConsentOutcome::Denied),
            ("printf 'approved\\nextra'", ConsentOutcome::Denied),
        ] {
            let mut child = Command::new("sh")
                .args(["-c", script])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            assert_eq!(
                wait_for_consent(&mut child, &request, &session)
                    .await
                    .unwrap(),
                expected
            );
            assert!(child.try_wait().unwrap().is_some());
        }
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        request.store(true, Ordering::Release);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_consent(&mut child, &request, &session),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, ConsentOutcome::Cancelled);
        assert!(
            child.try_wait().unwrap().is_some(),
            "cancelled UI must be reaped"
        );
    }

    fn object_path(value: &str) -> ObjectPath<'_> {
        ObjectPath::try_from(value).unwrap()
    }

    fn assert_error(err: zbus::Error, expected: &str) {
        match err {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), expected),
            err => panic!("Expected D-Bus method error, got {err}"),
        }
    }

    async fn claim_frontend(connection: &Connection) {
        assert_eq!(
            connection
                .request_name_with_flags(
                    FRONTEND_BUS_NAME,
                    fdo::RequestNameFlags::DoNotQueue.into(),
                )
                .await
                .unwrap(),
            fdo::RequestNameReply::PrimaryOwner
        );
    }

    #[test]
    fn validates_handle_namespaces() {
        assert!(
            require_path(
                &object_path("/org/freedesktop/portal/desktop/request/one"),
                REQUEST_PREFIX
            )
            .is_ok()
        );
        assert!(
            require_path(
                &object_path("/org/freedesktop/portal/desktop"),
                REQUEST_PREFIX
            )
            .is_err()
        );
        assert!(
            require_path(
                &object_path("/org/freedesktop/portal/desktop/session/one"),
                REQUEST_PREFIX
            )
            .is_err()
        );
    }

    #[test]
    fn session_registration_rolls_back_without_removing_a_reused_path() {
        let sessions = Sessions::default();
        let path = "/org/freedesktop/portal/desktop/session/registration/test";
        let pending =
            SessionRegistration::reserve(sessions.clone(), path, ":1.1".into(), "test.app".into())
                .unwrap();
        let closed = pending.closed.clone();
        assert!(sessions.lock().unwrap().contains_key(path));
        drop(pending);
        assert!(closed.load(Ordering::Acquire));
        assert!(sessions.lock().unwrap().is_empty());

        let creating =
            SessionRegistration::reserve(sessions.clone(), path, ":1.1".into(), "test.app".into())
                .unwrap();
        assert!(remove_session(&sessions, path, &creating.closed));
        assert!(creating.closed.load(Ordering::Acquire));
        let mut replacement =
            SessionRegistration::reserve(sessions.clone(), path, ":1.1".into(), "test.app".into())
                .unwrap();
        let replacement_closed = replacement.closed.clone();
        assert!(!remove_session(&sessions, path, &creating.closed));
        drop(creating);
        assert!(sessions.lock().unwrap().contains_key(path));
        replacement.committed = true;
        drop(replacement);
        assert!(!replacement_closed.load(Ordering::Acquire));
        assert!(remove_session(&sessions, path, &replacement_closed));
        assert!(sessions.lock().unwrap().is_empty());
    }

    /// Run explicitly: dbus-run-session -- cargo test -p aura-session portal_dbus_lifecycle -- --ignored
    #[tokio::test]
    #[ignore = "requires a private session bus; run under dbus-run-session"]
    async fn portal_dbus_lifecycle() {
        let _bus_lock = TEST_BUS_LOCK.lock().await;
        let service = serve().await.expect("backend must register on private bus");
        let client = Connection::session().await.unwrap();
        claim_frontend(&client).await;
        let proxy = zbus::Proxy::new(
            &client,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let source_types: u32 = proxy.get_property("AvailableSourceTypes").await.unwrap();
        assert_eq!(
            source_types, 0,
            "must not advertise capture that does not exist"
        );
        let introspect = zbus::fdo::IntrospectableProxy::builder(&client)
            .destination(BUS_NAME)
            .unwrap()
            .path(OBJECT_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let xml = introspect.introspect().await.unwrap();
        for method in ["CreateSession", "SelectSources", "Start"] {
            assert!(xml.contains(&format!("name=\"{method}\"")));
        }
        assert!(!xml.contains("StartScreencast"));
        // Exercise Request.Close independently of short-lived immediate calls.
        let cancel_path = object_path("/org/freedesktop/portal/desktop/request/test/cancel");
        let cancelled = begin_request(
            service._connection.object_server(),
            &cancel_path,
            &service.requests,
            client.unique_name().unwrap().as_str(),
        )
        .await
        .unwrap();
        let cancel_proxy = zbus::Proxy::new(
            &client,
            BUS_NAME,
            cancel_path.clone(),
            "org.freedesktop.impl.portal.Request",
        )
        .await
        .unwrap();
        let stranger = Connection::session().await.unwrap();
        let stranger_request = zbus::Proxy::new(
            &stranger,
            BUS_NAME,
            cancel_path,
            "org.freedesktop.impl.portal.Request",
        )
        .await
        .unwrap();
        let denied = stranger_request
            .call::<_, _, ()>("Close", &())
            .await
            .unwrap_err();
        assert_error(denied, "org.freedesktop.DBus.Error.AccessDenied");
        assert!(!cancelled.load(Ordering::Acquire));
        cancel_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
        assert!(cancelled.load(Ordering::Acquire));
        // Successful Start removes its request interface while its worker
        // retains the shared cancellation token; completion must not stop it.
        let completed_path = object_path("/org/freedesktop/portal/desktop/request/test/completed");
        let worker_stop = begin_request(
            service._connection.object_server(),
            &completed_path,
            &service.requests,
            client.unique_name().unwrap().as_str(),
        )
        .await
        .unwrap();
        finish_request(
            service._connection.object_server(),
            &service.requests,
            completed_path.as_str(),
            &worker_stop,
        )
        .await;
        assert!(!worker_stop.load(Ordering::Acquire));
        assert!(service.requests.lock().unwrap().is_empty());
        let replacement_stop = begin_request(
            service._connection.object_server(),
            &completed_path,
            &service.requests,
            client.unique_name().unwrap().as_str(),
        )
        .await
        .unwrap();
        finish_request(
            service._connection.object_server(),
            &service.requests,
            completed_path.as_str(),
            &worker_stop,
        )
        .await;
        let replacement_request = zbus::Proxy::new(
            &client,
            BUS_NAME,
            completed_path,
            "org.freedesktop.impl.portal.Request",
        )
        .await
        .unwrap();
        replacement_request
            .call::<_, _, ()>("Close", &())
            .await
            .unwrap();
        assert!(replacement_stop.load(Ordering::Acquire));
        assert!(!worker_stop.load(Ordering::Acquire));
        let request = object_path("/org/freedesktop/portal/desktop/request/test/create");
        let session = object_path("/org/freedesktop/portal/desktop/session/test/first");
        let response: Response = proxy
            .call(
                "CreateSession",
                &(&request, &session, "test.app", Options::new()),
            )
            .await
            .unwrap();
        assert_eq!(response.0, 0);
        let duplicate = proxy
            .call::<_, _, Response>(
                "CreateSession",
                &(&request, &session, "test.app", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(duplicate, "org.freedesktop.DBus.Error.InvalidArgs");
        let wrong_app = proxy
            .call::<_, _, Response>(
                "SelectSources",
                &(&request, &session, "other.app", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(wrong_app, "org.freedesktop.DBus.Error.AccessDenied");
        let unsupported = proxy
            .call::<_, _, Response>(
                "SelectSources",
                &(&request, &session, "test.app", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(unsupported, "org.freedesktop.DBus.Error.NotSupported");
        let unsupported = proxy
            .call::<_, _, Response>(
                "Start",
                &(&request, &session, "test.app", "", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(unsupported, "org.freedesktop.DBus.Error.NotSupported");
        let session_proxy = zbus::Proxy::new(
            &client,
            BUS_NAME,
            session.clone(),
            "org.freedesktop.impl.portal.Session",
        )
        .await
        .unwrap();
        let stranger_session = zbus::Proxy::new(
            &stranger,
            BUS_NAME,
            session.clone(),
            "org.freedesktop.impl.portal.Session",
        )
        .await
        .unwrap();
        let denied = stranger_session
            .call::<_, _, ()>("Close", &())
            .await
            .unwrap_err();
        assert_error(denied, "org.freedesktop.DBus.Error.AccessDenied");
        session_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
        let closed = proxy
            .call::<_, _, Response>(
                "Start",
                &(&request, &session, "test.app", "", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(closed, "org.freedesktop.DBus.Error.InvalidArgs");
        let response: Response = proxy
            .call(
                "CreateSession",
                &(&request, &session, "test.app", Options::new()),
            )
            .await
            .unwrap();
        assert_eq!(
            response.0, 0,
            "Close must deregister the session path for reuse"
        );
        client.close().await.unwrap();
        // A different client must eventually be able to reuse the path after
        // NameOwnerChanged cleanup, rather than inheriting a stale session.
        let other = Connection::session().await.unwrap();
        claim_frontend(&other).await;
        let other_proxy = zbus::Proxy::new(
            &other,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let result = other_proxy
                    .call::<_, _, Response>(
                        "CreateSession",
                        &(&request, &session, "test.app", Options::new()),
                    )
                    .await;
                if result.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("disconnect must remove owned sessions");
        other.release_name(FRONTEND_BUS_NAME).await.unwrap();
        claim_frontend(&stranger).await;
        let old_frontend = other_proxy
            .call::<_, _, Response>(
                "Start",
                &(&request, &session, "test.app", "", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(old_frontend, "org.freedesktop.DBus.Error.AccessDenied");
        let new_frontend = zbus::Proxy::new(
            &stranger,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let inherited_session = new_frontend
            .call::<_, _, Response>(
                "Start",
                &(&request, &session, "test.app", "", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(inherited_session, "org.freedesktop.DBus.Error.AccessDenied");
    }

    #[tokio::test]
    #[ignore = "requires a private session bus; run under dbus-run-session"]
    async fn portal_dbus_consent_denial_and_request_cancellation() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "aura-consent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = directory.join("consent-ui");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'opened\\n' > \"$XDG_RUNTIME_DIR/ui-opened\"\nprintf 'denied\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _bus_lock = TEST_BUS_LOCK.lock().await;
        // A deliberately unreachable capture source makes an accidental
        // consent bypass fail instead of fabricating a PipeWire stream.
        let backend = ScreenCast {
            capture: Some(CaptureConfig {
                socket: directory.join("no-capture.sock"),
                token: "0".repeat(64),
                remote: None,
            }),
            consent: Some(ConsentUi {
                program: executable.clone().into(),
                wayland_display: "unused-test-display".into(),
                runtime_dir: directory.clone(),
            }),
            ..Default::default()
        };
        let service = serve_backend(backend).await.unwrap();
        let client = Connection::session().await.unwrap();
        claim_frontend(&client).await;
        let proxy = zbus::Proxy::new(
            &client,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let create = object_path("/org/freedesktop/portal/desktop/request/consent/create");
        let select = object_path("/org/freedesktop/portal/desktop/request/consent/select");
        let start = object_path("/org/freedesktop/portal/desktop/request/consent/start");
        let session = object_path("/org/freedesktop/portal/desktop/session/consent/first");
        let stranger = Connection::session().await.unwrap();
        let stranger_proxy = zbus::Proxy::new(
            &stranger,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let sources: u32 = stranger_proxy
            .get_property("AvailableSourceTypes")
            .await
            .unwrap();
        assert_eq!(sources, 1, "capability metadata remains public");
        let denied = stranger_proxy
            .call::<_, _, Response>(
                "CreateSession",
                &(&create, &session, "test.app", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(denied, "org.freedesktop.DBus.Error.AccessDenied");
        assert!(service.sessions.lock().unwrap().is_empty());
        let response: Response = proxy
            .call(
                "CreateSession",
                &(&create, &session, "test.app", Options::new()),
            )
            .await
            .unwrap();
        assert_eq!(response.0, 0);
        let denied = stranger_proxy
            .call::<_, _, Response>(
                "SelectSources",
                &(&select, &session, "test.app", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(denied, "org.freedesktop.DBus.Error.AccessDenied");
        assert!(!service.sessions.lock().unwrap()[session.as_str()].selected);
        let response: Response = proxy
            .call(
                "SelectSources",
                &(&select, &session, "test.app", Options::new()),
            )
            .await
            .unwrap();
        assert_eq!(response.0, 0);
        let denied = stranger_proxy
            .call::<_, _, Response>("Start", &(&start, &session, "test.app", "", Options::new()))
            .await
            .unwrap_err();
        assert_error(denied, "org.freedesktop.DBus.Error.AccessDenied");
        assert!(!directory.join("ui-opened").exists());
        assert!(service.requests.lock().unwrap().is_empty());
        assert!(
            service.sessions.lock().unwrap()[session.as_str()]
                .capture
                .is_none()
        );
        let response: Response = proxy
            .call("Start", &(&start, &session, "test.app", "", Options::new()))
            .await
            .unwrap();
        assert_eq!(response.0, 1, "UI denial must not start capture");
        assert!(response.1.is_empty());
        assert!(
            service.sessions.lock().unwrap()[session.as_str()]
                .capture
                .is_none()
        );

        std::fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let start_proxy = zbus::Proxy::new_owned(
            client.clone(),
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let start_path = start.clone().to_owned();
        let session_path = session.clone().to_owned();
        let pending = tokio::spawn(async move {
            start_proxy
                .call::<_, _, Response>(
                    "Start",
                    &(start_path, session_path, "test.app", "", Options::new()),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !service
                .requests
                .lock()
                .unwrap()
                .contains_key(start.as_str())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let duplicate_start =
            object_path("/org/freedesktop/portal/desktop/request/consent/duplicate");
        let duplicate = proxy
            .call::<_, _, Response>(
                "Start",
                &(&duplicate_start, &session, "test.app", "", Options::new()),
            )
            .await
            .unwrap_err();
        assert_error(duplicate, "org.freedesktop.DBus.Error.InvalidArgs");
        assert!(
            service.sessions.lock().unwrap()[session.as_str()]
                .starting
                .is_some(),
            "a rejected concurrent Start must not release the active claim"
        );
        let request_proxy = zbus::Proxy::new(
            &client,
            BUS_NAME,
            start.clone(),
            "org.freedesktop.impl.portal.Request",
        )
        .await
        .unwrap();
        request_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.0, 1);
        assert!(response.1.is_empty());
        assert!(
            !service
                .requests
                .lock()
                .unwrap()
                .contains_key(start.as_str())
        );
        assert!(
            service.sessions.lock().unwrap()[session.as_str()]
                .starting
                .is_none()
        );
        assert!(
            service.sessions.lock().unwrap()[session.as_str()]
                .capture
                .is_none()
        );
        // Test-only consent output reaches the real capture startup path. A
        // missing source must remain a failure, even though worker teardown
        // also sets the shared stop flag.
        std::fs::write(&executable, "#!/bin/sh\nprintf 'approved\\n'\n").unwrap();
        let failed = proxy
            .call::<_, _, Response>("Start", &(&start, &session, "test.app", "", Options::new()))
            .await
            .unwrap_err();
        assert_error(failed, "org.freedesktop.DBus.Error.Failed");

        // Hold the capture source before its first frame, then cancel after
        // the worker has connected. No PipeWire node can be created here.
        let source = tokio::net::UnixListener::bind(directory.join("no-capture.sock")).unwrap();
        let start_proxy = zbus::Proxy::new_owned(
            client.clone(),
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let start_path = start.clone().to_owned();
        let session_path = session.clone().to_owned();
        let pending = tokio::spawn(async move {
            start_proxy
                .call::<_, _, Response>(
                    "Start",
                    &(start_path, session_path, "test.app", "", Options::new()),
                )
                .await
        });
        let (_capture_peer, _) = tokio::time::timeout(Duration::from_secs(2), source.accept())
            .await
            .unwrap()
            .unwrap();
        request_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.0, 1);
        assert!(response.1.is_empty());
        assert!(
            service.sessions.lock().unwrap()[session.as_str()]
                .capture
                .is_none()
        );
        let session_proxy = zbus::Proxy::new(
            &client,
            BUS_NAME,
            session,
            "org.freedesktop.impl.portal.Session",
        )
        .await
        .unwrap();
        session_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
        drop(service);
        let _ = std::fs::remove_dir_all(directory);
    }
}
