//! Exercise the exported services on a real isolated session bus:
//! dbus-run-session -- cargo run -p aura-shell --example dbus_smoke
use aura_shell::ipc::{self, SharedState};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;
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
    assert_eq!(capabilities, vec!["body"]);
    let first: u32 = proxy
        .call(
            "Notify",
            &(
                "test-app",
                0u32,
                "",
                "First",
                "Plain text body",
                Vec::<String>::new(),
                HashMap::<String, OwnedValue>::new(),
                0i32,
            ),
        )
        .await?;
    assert_ne!(first, 0);
    let replacement: u32 = proxy
        .call(
            "Notify",
            &(
                "test-app",
                first,
                "",
                "Replacement",
                "",
                Vec::<String>::new(),
                HashMap::<String, OwnedValue>::new(),
                0i32,
            ),
        )
        .await?;
    assert_eq!(first, replacement);
    assert_eq!(state.lock().unwrap().notifications.len(), 1);
    assert_eq!(
        state.lock().unwrap().notifications[&first].summary,
        "Replacement"
    );
    let _: () = proxy.call("CloseNotification", &(first,)).await?;
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
    let expiry: u32 = proxy
        .call(
            "Notify",
            &(
                "test-app",
                0u32,
                "",
                "Expires",
                "",
                Vec::<String>::new(),
                HashMap::<String, OwnedValue>::new(),
                20i32,
            ),
        )
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(!state.lock().unwrap().notifications.contains_key(&expiry));
    let result: Result<String, _> = shell.call("SetLayoutMode", &("invalid",)).await;
    assert!(result.is_err());
    println!("D-Bus notifications: capability, replacement, close, expiry, DND, validation passed");
    Ok(())
}
