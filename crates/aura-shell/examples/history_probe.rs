//! Persistent notification owner for the private-display notification history check.
use std::{collections::HashMap, future::poll_fn, io::Write, time::Duration};
use zbus::{export::futures_core::Stream, zvariant::OwnedValue};

type ProbeResult = Result<(), Box<dyn std::error::Error>>;

struct Options {
    summary: String,
    body: String,
    resident: bool,
    dismiss: bool,
    expired: bool,
    hold_after_close: Duration,
}

impl Options {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        const USAGE: &str = "history_probe [--summary LABEL] [--body TEXT] [--resident] [--expect-dismiss | --expect-expired] [--hold-after-close-ms N]";
        let mut options = Self {
            summary: "History notification".into(),
            body: "Exercise a real notification lifecycle and its archived detail.".into(),
            resident: false,
            dismiss: false,
            expired: false,
            hold_after_close: Duration::from_millis(3000),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--summary" => options.summary = args.next().ok_or(USAGE)?,
                "--body" => options.body = args.next().ok_or(USAGE)?,
                "--resident" => options.resident = true,
                "--expect-dismiss" => options.dismiss = true,
                "--expect-expired" => options.expired = true,
                "--hold-after-close-ms" => {
                    let milliseconds: u64 = args.next().ok_or(USAGE)?.parse()?;
                    if milliseconds > 30_000 {
                        return Err("post-close hold must be at most 30000 ms".into());
                    }
                    options.hold_after_close = Duration::from_millis(milliseconds);
                }
                _ => return Err(USAGE.into()),
            }
        }
        if options.expired && (options.dismiss || options.resident) {
            return Err("incompatible history probe options".into());
        }
        Ok(options)
    }
}

#[tokio::main]
async fn main() -> ProbeResult {
    let options = Options::parse()?;
    tokio::time::timeout(Duration::from_secs(45), probe(options))
        .await
        .map_err(|_| "notification history workflow timed out after 45 seconds")?
}

async fn probe(options: Options) -> ProbeResult {
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
    // One stream preserves ordering, and subscribing first prevents fast actions racing Notify.
    let mut signals = proxy.receive_all_signals().await?;
    let action_pairs: Vec<String> = ["open", "Open"].into_iter().map(String::from).collect();
    let hints =
        HashMap::<&str, OwnedValue>::from([("resident", OwnedValue::from(options.resident))]);
    let id: u32 = proxy
        .call(
            "Notify",
            &(
                "Aura History Probe",
                0u32,
                "",
                options.summary.as_str(),
                options.body.as_str(),
                action_pairs,
                hints,
                if options.expired { 500i32 } else { 0i32 },
            ),
        )
        .await?;
    if id == 0 {
        return Err("Notify returned a zero ID".into());
    }
    println!("HISTORY_READY id={id}");
    std::io::stdout().flush()?;
    let mut invoked = 0;
    let mut closed_at = None;
    loop {
        let next = poll_fn(|context| std::pin::Pin::new(&mut signals).poll_next(context));
        let message = if let Some(deadline) = closed_at {
            match tokio::time::timeout_at(deadline, next).await {
                Ok(message) => message,
                Err(_) => {
                    println!("HISTORY_FINISHED id={id} actions={invoked}");
                    std::io::stdout().flush()?;
                    return Ok(());
                }
            }
        } else {
            next.await
        }
        .ok_or("notification signal stream closed")?;
        let header = message.header();
        let member = header.member().map(|name| name.as_str());
        if !matches!(member, Some("ActionInvoked" | "NotificationClosed")) {
            continue;
        }
        if message.header().destination().map(|name| name.as_str()) != Some(owner.as_str()) {
            return Err("notification signal was not directed to the originating client".into());
        }
        if closed_at.is_some() {
            return Err(
                "notification action or close arrived after the notification closed".into(),
            );
        }
        match member {
            Some("ActionInvoked") => {
                let (action_id, key): (u32, String) = message.body().deserialize()?;
                if action_id != id
                    || key != "open"
                    || options.dismiss
                    || options.expired
                    || invoked != 0
                {
                    return Err(format!("unexpected action id={action_id} key={key}").into());
                }
                invoked += 1;
                println!("HISTORY_ACTION id={id} key={key}");
                std::io::stdout().flush()?;
            }
            Some("NotificationClosed") => {
                let (closed_id, reason): (u32, u32) = message.body().deserialize()?;
                let expected_reason = if options.expired { 1 } else { 2 };
                let expected_actions = u32::from(!options.dismiss && !options.expired);
                if closed_id != id || reason != expected_reason || invoked != expected_actions {
                    return Err(format!(
                        "unexpected close id={closed_id} reason={reason} actions={invoked}"
                    )
                    .into());
                }
                println!("HISTORY_CLOSED id={id} reason={reason} actions={invoked}");
                std::io::stdout().flush()?;
                closed_at = Some(tokio::time::Instant::now() + options.hold_after_close);
            }
            _ => unreachable!("only notification lifecycle signals are handled"),
        }
    }
}
