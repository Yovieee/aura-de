//! Persistent notification caller for the private-display desktop controls check.
use std::{collections::HashMap, future::poll_fn, io::Write, time::Duration};
use zbus::{export::futures_core::Stream, zvariant::OwnedValue};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut resident = false;
    let mut dismiss = false;
    let mut expired = false;
    let mut many = false;
    let mut default_action = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--resident" => resident = true,
            "--expect-dismiss" => dismiss = true,
            "--expect-expired" => expired = true,
            "--many" => many = true,
            "--default-action" => default_action = true,
            _ => return Err("notification_probe [--resident] [--expect-dismiss | --expect-expired] [--many | --default-action]".into()),
        }
    }
    if (dismiss && expired) || (many && default_action) || (resident && expired) {
        return Err("incompatible notification probe options".into());
    }
    let connection = zbus::Connection::session().await?;
    let owner = connection
        .unique_name()
        .ok_or("D-Bus unique name missing")?
        .as_str()
        .to_owned();
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).await?;
    if !capabilities
        .iter()
        .any(|capability| capability == "actions")
    {
        return Err("notification service does not advertise actions".into());
    }
    // Install signal subscriptions before Notify so fast UI actions cannot race them.
    let mut signals = proxy.receive_all_signals().await?;
    let expected_key = if many {
        "six"
    } else if default_action {
        "default"
    } else {
        "open"
    };
    let action_pairs: Vec<String> = if many {
        [
            "one", "One", "two", "Two", "three", "Three", "four", "Four", "five", "Five", "six",
            "Six",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    } else {
        [expected_key, "Open"]
            .into_iter()
            .map(String::from)
            .collect()
    };
    let hints = HashMap::<&str, OwnedValue>::from([("resident", OwnedValue::from(resident))]);
    let id: u32 = proxy
        .call(
            "Notify",
            &(
                "Aura Controls Probe",
                0u32,
                "",
                "Desktop notification",
                "Exercise a real notification action or dismissal.",
                action_pairs,
                hints,
                if expired { 500i32 } else { 0i32 },
            ),
        )
        .await?;
    if id == 0 {
        return Err("Notify returned a zero ID".into());
    }
    println!("PROBE_READY id={id} resident={resident}");
    std::io::stdout().flush()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut invoked = 0;
    loop {
        let message = tokio::time::timeout_at(
            deadline,
            poll_fn(|context| std::pin::Pin::new(&mut signals).poll_next(context)),
        )
        .await
        .map_err(|_| "notification action/close timed out")?
        .ok_or("notification signal stream closed")?;
        if message.header().destination().map(|name| name.as_str()) != Some(owner.as_str()) {
            return Err("notification signal was not directed to the originating client".into());
        }
        match message.header().member().map(|name| name.as_str()) {
            Some("ActionInvoked") => {
                let (action_id, key): (u32, String) = message.body().deserialize()?;
                if action_id != id || key != expected_key || dismiss || expired {
                    return Err(format!("unexpected action id={action_id} key={key}").into());
                }
                invoked += 1;
                println!("PROBE_ACTION id={id} key={key}");
                std::io::stdout().flush()?;
            }
            Some("NotificationClosed") => {
                let (closed_id, reason): (u32, u32) = message.body().deserialize()?;
                let expected_reason = if expired { 1 } else { 2 };
                if closed_id != id || reason != expected_reason {
                    return Err(format!("unexpected close id={closed_id} reason={reason}").into());
                }
                if (dismiss || expired) && invoked != 0 || (!dismiss && !expired && invoked == 0) {
                    return Err("close did not follow the expected action lifecycle".into());
                }
                println!("PROBE_CLOSED id={id} reason={reason} actions={invoked}");
                return Ok(());
            }
            _ => {}
        }
    }
}
