//! A truthful ScreenCast backend: real D-Bus/session lifecycle, capture unavailable.
//!
//! The backend advertises no capture sources until the compositor has a capture
//! transport. It never fabricates PipeWire node IDs or accepts capture requests.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_lite::StreamExt;
use tokio::{sync::Mutex, task::JoinHandle};
use zbus::{
    Connection, fdo,
    message::Header,
    object_server::{ObjectServer, SignalEmitter},
    zvariant::{ObjectPath, OwnedValue},
};

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.aura";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request/";
const SESSION_PREFIX: &str = "/org/freedesktop/portal/desktop/session/";
const MAX_SESSIONS: usize = 256;
const CAPTURE_UNAVAILABLE: &str =
    "Aura has no compositor-to-PipeWire capture transport; screen sharing is not supported yet";

type Options = HashMap<String, OwnedValue>;
type Response = (u32, Options);
type Sessions = Arc<Mutex<HashMap<String, SessionRecord>>>;

#[derive(Clone)]
struct SessionRecord {
    owner: String,
    app_id: String,
}

/// Keeps both the D-Bus connection and the disconnect-cleanup task alive.
/// Dropping the service stops the task, allowing the connection to be released.
pub struct PortalService {
    _connection: Connection,
    cleanup: JoinHandle<()>,
}

impl Drop for PortalService {
    fn drop(&mut self) {
        self.cleanup.abort();
    }
}

#[derive(Default)]
struct ScreenCast {
    sessions: Sessions,
}

fn sender(header: &Header<'_>) -> fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| fdo::Error::AccessDenied("A bus sender is required".into()))
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

async fn authorize(
    sessions: &Sessions,
    path: &ObjectPath<'_>,
    app_id: &str,
    owner: &str,
) -> fdo::Result<()> {
    let registry = sessions.lock().await;
    let session = registry
        .get(path.as_str())
        .ok_or_else(|| fdo::Error::InvalidArgs("Unknown or closed portal session".into()))?;
    if session.owner != owner || session.app_id != app_id {
        return Err(fdo::Error::AccessDenied(
            "Portal session belongs to another sender or application".into(),
        ));
    }
    Ok(())
}

struct PortalRequest {
    path: String,
    owner: String,
    cancelled: Arc<AtomicBool>,
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
        finish_request(server, &self.path).await;
        Ok(())
    }
}

async fn begin_request(
    server: &ObjectServer,
    path: &ObjectPath<'_>,
    owner: &str,
) -> fdo::Result<Arc<AtomicBool>> {
    require_path(path, REQUEST_PREFIX)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let added = server
        .at(
            path.clone(),
            PortalRequest {
                path: path.to_string(),
                owner: owner.into(),
                cancelled: cancelled.clone(),
            },
        )
        .await
        .map_err(|err| fdo::Error::Failed(err.to_string()))?;
    if !added {
        return Err(fdo::Error::InvalidArgs(
            "Portal request handle is already in use".into(),
        ));
    }
    Ok(cancelled)
}

async fn finish_request(server: &ObjectServer, path: &str) {
    // Close may already have removed a cancelled request.
    let _ = server.remove::<PortalRequest, _>(path).await;
}

struct PortalSession {
    path: String,
    owner: String,
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
        self.sessions.lock().await.remove(&self.path);
        server
            .remove::<PortalSession, _>(self.path.as_str())
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        Ok(())
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<Response> {
        require_path(&handle, REQUEST_PREFIX)?;
        require_path(&session_handle, SESSION_PREFIX)?;
        let owner = sender(&header)?;
        let mut sessions = self.sessions.lock().await;
        if sessions.len() >= MAX_SESSIONS {
            return Err(fdo::Error::LimitsExceeded(
                "Too many active portal sessions".into(),
            ));
        }
        if sessions.contains_key(session_handle.as_str()) {
            return Err(fdo::Error::InvalidArgs(
                "Portal session handle is already in use".into(),
            ));
        }
        let cancelled = begin_request(server, &handle, &owner).await?;
        let result = server
            .at(
                session_handle.clone(),
                PortalSession {
                    path: session_handle.to_string(),
                    owner: owner.clone(),
                    sessions: self.sessions.clone(),
                },
            )
            .await;
        finish_request(server, handle.as_str()).await;
        let added = result.map_err(|err| fdo::Error::Failed(err.to_string()))?;
        if !added {
            return Err(fdo::Error::InvalidArgs(
                "Portal session path is already registered".into(),
            ));
        }
        if cancelled.load(Ordering::Acquire) {
            let _ = server
                .remove::<PortalSession, _>(session_handle.clone())
                .await;
            return Ok((1, Options::new()));
        }
        sessions.insert(
            session_handle.to_string(),
            SessionRecord {
                owner,
                app_id: app_id.into(),
            },
        );
        Ok((0, Options::new()))
    }

    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<Response> {
        require_path(&handle, REQUEST_PREFIX)?;
        let owner = sender(&header)?;
        authorize(&self.sessions, &session_handle, app_id, &owner).await?;
        let cancelled = begin_request(server, &handle, &owner).await?;
        finish_request(server, handle.as_str()).await;
        if cancelled.load(Ordering::Acquire) {
            return Ok((1, Options::new()));
        }
        Err(fdo::Error::NotSupported(CAPTURE_UNAVAILABLE.into()))
    }

    // D-Bus fixes this method's five wire arguments; header/server are injected.
    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: Options,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<Response> {
        require_path(&handle, REQUEST_PREFIX)?;
        let owner = sender(&header)?;
        authorize(&self.sessions, &session_handle, app_id, &owner).await?;
        let cancelled = begin_request(server, &handle, &owner).await?;
        finish_request(server, handle.as_str()).await;
        if cancelled.load(Ordering::Acquire) {
            return Ok((1, Options::new()));
        }
        Err(fdo::Error::NotSupported(CAPTURE_UNAVAILABLE.into()))
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        0
    }

    // Advertise the base interface version only, with no restore/persistence
    // extensions. Capability properties truthfully report capture unavailable.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Register a ScreenCast backend and clean up sessions when their frontend exits.
pub async fn serve() -> zbus::Result<PortalService> {
    let backend = ScreenCast::default();
    let sessions = backend.sessions.clone();
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, backend)?
        .build()
        .await?;
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;
    let cleanup_connection = connection.clone();
    let cleanup = tokio::spawn(async move {
        while let Some(change) = owner_changes.next().await {
            let Ok(args) = change.args() else { continue };
            if args.new_owner().is_some() || !args.name().as_str().starts_with(':') {
                continue;
            }
            let removed = {
                let mut registry = sessions.lock().await;
                let mut removed = Vec::new();
                registry.retain(|path, record| {
                    if record.owner == args.name().as_str() {
                        removed.push(path.clone());
                        false
                    } else {
                        true
                    }
                });
                removed
            };
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
        "ScreenCast control service registered; capture unavailable"
    );
    Ok(PortalService {
        _connection: connection,
        cleanup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_path(value: &str) -> ObjectPath<'_> {
        ObjectPath::try_from(value).unwrap()
    }

    fn assert_error(err: zbus::Error, expected: &str) {
        match err {
            zbus::Error::MethodError(name, _, _) => assert_eq!(name.as_str(), expected),
            err => panic!("Expected D-Bus method error, got {err}"),
        }
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

    /// Run explicitly: dbus-run-session -- cargo test -p aura-session portal_dbus_lifecycle -- --ignored
    #[tokio::test]
    #[ignore = "requires a private session bus; run under dbus-run-session"]
    async fn portal_dbus_lifecycle() {
        let service = serve().await.expect("backend must register on private bus");
        let client = Connection::session().await.unwrap();
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
    }
}
