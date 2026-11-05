//! Exercise notification ownership/actions on a real isolated session bus:
//! dbus-run-session -- cargo run -p aura-shell --example dbus_smoke --locked
use aura_shell::ipc::{self, SharedState};
use std::{collections::HashMap, pin::Pin, time::Duration};
use zbus::{export::futures_core::Stream, proxy::SignalStream, zvariant::OwnedValue};

async fn next(stream: &mut SignalStream<'_>) -> Result<zbus::Message, Box<dyn std::error::Error>> {
    tokio::time::timeout(
        Duration::from_secs(2),
        std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)),
    )
    .await?
    .ok_or_else(|| "signal stream ended".into())
}
fn directed(message: &zbus::Message, connection: &zbus::Connection) {
    assert_eq!(
        message.header().destination().map(ToString::to_string),
        connection.unique_name().map(ToString::to_string)
    );
}
async fn notify(
    proxy: &zbus::Proxy<'_>,
    replaces: u32,
    actions: Vec<&str>,
    resident: bool,
    timeout: i32,
) -> zbus::Result<u32> {
    proxy
        .call(
            "Notify",
            &(
                "test-app",
                replaces,
                "",
                "Summary",
                "Plain text body",
                actions,
                HashMap::from([("resident", OwnedValue::from(resident))]),
                timeout,
            ),
        )
        .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = SharedState::default();
    ipc::start_services(state.clone()).map_err(std::io::Error::other)?;
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).await?;
    assert_eq!(capabilities, vec!["body", "actions", "persistence"]);
    let mut actions = proxy.receive_signal("ActionInvoked").await?;
    let mut closed = proxy.receive_signal("NotificationClosed").await?;
    let foreign = zbus::Connection::session().await?;
    let foreign_proxy = zbus::Proxy::new(
        &foreign,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let mut foreign_actions = foreign_proxy.receive_signal("ActionInvoked").await?;
    let first = notify(&proxy, 0, vec!["open", "Open"], false, 0).await?;
    assert_ne!(first, 0);
    let old_generation = state.lock().unwrap().notifications[&first].generation;
    let denied = notify(&foreign_proxy, first, vec![], false, 0)
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("AccessDenied"));
    let denied: Result<(), _> = foreign_proxy.call("CloseNotification", &(first,)).await;
    assert!(denied.unwrap_err().to_string().contains("AccessDenied"));
    assert_eq!(
        notify(&proxy, first, vec!["open", "Open replacement"], false, 0).await?,
        first
    );
    let generation = state.lock().unwrap().notifications[&first].generation;
    assert_ne!(old_generation, generation);
    assert!(!ipc::enqueue_action(&state, first, old_generation, "open"));
    assert!(!ipc::enqueue_dismiss(&state, first, old_generation));
    assert!(ipc::enqueue_action(&state, first, generation, "open"));
    let action = next(&mut actions).await?;
    directed(&action, &connection);
    assert_eq!(
        action.body().deserialize::<(u32, String)>()?,
        (first, "open".into())
    );
    let close = next(&mut closed).await?;
    directed(&close, &connection);
    assert_eq!(close.body().deserialize::<(u32, u32)>()?, (first, 2));
    assert!(!state.lock().unwrap().notifications.contains_key(&first));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            std::future::poll_fn(|cx| Pin::new(&mut foreign_actions).poll_next(cx))
        )
        .await
        .is_err()
    );

    let resident = notify(&proxy, 0, vec!["default", ""], true, 0).await?;
    let generation = state.lock().unwrap().notifications[&resident].generation;
    assert!(ipc::enqueue_action(&state, resident, generation, "default"));
    let action = next(&mut actions).await?;
    directed(&action, &connection);
    assert_eq!(
        action.body().deserialize::<(u32, String)>()?,
        (resident, "default".into())
    );
    assert!(state.lock().unwrap().notifications.contains_key(&resident));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            std::future::poll_fn(|cx| Pin::new(&mut closed).poll_next(cx))
        )
        .await
        .is_err()
    );
    assert!(ipc::enqueue_dismiss(&state, resident, generation));
    let close = next(&mut closed).await?;
    directed(&close, &connection);
    assert_eq!(close.body().deserialize::<(u32, u32)>()?, (resident, 2));

    let programmatic = notify(&proxy, 0, vec![], false, 0).await?;
    let _: () = proxy.call("CloseNotification", &(programmatic,)).await?;
    let close = next(&mut closed).await?;
    directed(&close, &connection);
    assert_eq!(close.body().deserialize::<(u32, u32)>()?, (programmatic, 3));
    let expiry = notify(&proxy, 0, vec![], false, 20).await?;
    let close = next(&mut closed).await?;
    directed(&close, &connection);
    assert_eq!(close.body().deserialize::<(u32, u32)>()?, (expiry, 1));
    assert!(state.lock().unwrap().notifications.is_empty());
    let malformed = notify(&proxy, 0, vec!["open"], false, 0).await.unwrap_err();
    assert!(malformed.to_string().contains("InvalidArgs"));
    assert!(state.lock().unwrap().notifications.is_empty());
    let shell = zbus::Proxy::new(
        &connection,
        "org.aura.Shell1",
        "/org/aura/Shell1",
        "org.aura.Shell1",
    )
    .await?;
    shell.set_property("DoNotDisturb", true).await?;
    assert!(shell.get_property::<bool>("DoNotDisturb").await?);
    assert!(state.lock().unwrap().do_not_disturb);
    let result: Result<String, _> = shell.call("SetLayoutMode", &("invalid",)).await;
    assert!(result.is_err());
    println!(
        "D-Bus notifications: directed action/close signals, ownership, replacement generation, resident, dismissal, expiry, DND and validation passed"
    );
    Ok(())
}
