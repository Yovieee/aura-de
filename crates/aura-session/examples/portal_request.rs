//! Portal frontend emulation for an isolated test bus. It refuses to replace an
//! existing frontend. Consent must be supplied through the graphical overlay;
//! this executable has no automatic approval path.
use std::{collections::HashMap, io::Write, time::Duration};
use zbus::{
    Connection,
    zvariant::{ObjectPath, OwnedValue, Value},
};
type Options = HashMap<String, OwnedValue>;
type Response = (u32, Options);
const BUS: &str = "org.freedesktop.impl.portal.desktop.aura";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.ScreenCast";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = "org.aura.ScreenCastSmoke".to_string();
    let mut denied = false;
    let mut cancel = false;
    let mut hold = 30_u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app-id" => app = args.next().ok_or("--app-id requires a value")?,
            "--expect-denied" => denied = true,
            "--cancel" => cancel = true,
            "--hold-seconds" => {
                hold = args
                    .next()
                    .ok_or("--hold-seconds requires a value")?
                    .parse()?
            }
            "--help" => {
                println!(
                    "portal_request [--app-id NAME] [--expect-denied | --cancel] [--hold-seconds 1..300]\nThe real consent overlay must be operated by the user or isolated X11 input test."
                );
                return Ok(());
            }
            other => return Err(format!("unknown option: {other}").into()),
        }
    }
    if !(1..=300).contains(&hold) {
        return Err("--hold-seconds must be between1 and300".into());
    }
    let connection = Connection::session().await?;
    let claimed = connection
        .request_name_with_flags(
            "org.freedesktop.portal.Desktop",
            zbus::fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await?;
    if !matches!(
        claimed,
        zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner
    ) {
        return Err(
            "A portal frontend already owns this bus; use an isolated test session bus".into(),
        );
    }
    let proxy = zbus::Proxy::new(&connection, BUS, PATH, INTERFACE).await?;
    let sources: u32 = proxy.get_property("AvailableSourceTypes").await?;
    if sources & 1 == 0 {
        return Err("Portal does not advertise a ready monitor capture source".into());
    }
    let token: String = connection
        .unique_name()
        .ok_or("missing bus unique name")?
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let request_prefix = format!("/org/freedesktop/portal/desktop/request/{token}");
    let session = ObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/session/{token}/capture"
    ))?;
    let create = ObjectPath::try_from(format!("{request_prefix}/create"))?;
    let select = ObjectPath::try_from(format!("{request_prefix}/select"))?;
    let start = ObjectPath::try_from(format!("{request_prefix}/start"))?;
    let response: Response = proxy
        .call("CreateSession", &(&create, &session, &app, Options::new()))
        .await?;
    if response.0 != 0 {
        return Err("CreateSession did not succeed".into());
    }
    let response: Response = proxy
        .call(
            "SelectSources",
            &(
                &select,
                &session,
                &app,
                Options::from([
                    ("types".into(), OwnedValue::from(1_u32)),
                    ("cursor_mode".into(), OwnedValue::from(1_u32)),
                    ("multiple".into(), OwnedValue::from(false)),
                ]),
            ),
        )
        .await?;
    if response.0 != 0 {
        return Err("SelectSources did not succeed".into());
    }
    println!("PORTAL_PENDING app={app}");
    std::io::stdout().flush()?;
    let start_args = (&start, &session, &app, "", Options::new());
    let pending = proxy.call::<_, _, Response>("Start", &start_args);
    let response = if cancel {
        let request = zbus::Proxy::new(
            &connection,
            BUS,
            start.clone(),
            "org.freedesktop.impl.portal.Request",
        )
        .await?;
        tokio::pin!(pending);
        tokio::select! {
            response=&mut pending=>return Err(format!("Start completed before Request.Close: {response:?}").into()),
            ()=tokio::time::sleep(Duration::from_millis(350))=>{},
        }
        request.call::<_, _, ()>("Close", &()).await?;
        tokio::time::timeout(Duration::from_secs(3), pending).await??
    } else {
        pending.await?
    };
    if denied || cancel {
        if response.0 != 1 || !response.1.is_empty() {
            return Err("Consent rejection/cancellation unexpectedly returned a stream".into());
        }
        println!(
            "{}",
            if cancel {
                "PORTAL_CANCELLED"
            } else {
                "PORTAL_DENIED"
            }
        );
    } else {
        if response.0 != 0 {
            return Err("Screen sharing was not approved".into());
        }
        let streams: Vec<(u32, Options)> = response
            .1
            .get("streams")
            .ok_or("Start omitted streams")?
            .try_clone()?
            .try_into()?;
        if streams.len() != 1 || streams[0].0 == u32::MAX || streams[0].0 == 0 {
            return Err("Start did not return a real single-stream node".into());
        }
        let size_value = streams[0]
            .1
            .get("size")
            .ok_or("Start omitted size")?
            .try_clone()?;
        let size: (i32, i32) = Value::from(size_value).try_into()?;
        println!(
            "PORTAL_STREAM node_id={} width={} height={}",
            streams[0].0, size.0, size.1
        );
        std::io::stdout().flush()?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {()=tokio::time::sleep(Duration::from_secs(hold))=>{},_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    let session_proxy = zbus::Proxy::new(
        &connection,
        BUS,
        session,
        "org.freedesktop.impl.portal.Session",
    )
    .await?;
    match session_proxy.call::<_, _, ()>("Close", &()).await {
        Ok(()) => {}
        // The compositor's Stop control may already have revoked this stream
        // and removed its session before the fixture receives SIGTERM.
        Err(zbus::Error::MethodError(name, _, _))
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.UnknownObject"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
            ) => {}
        Err(error) => return Err(error.into()),
    }
    println!("PORTAL_CLOSED");
    Ok(())
}
