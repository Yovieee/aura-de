//! Real org.freedesktop.Application receiver for private-bus integration checks.
use std::{collections::HashMap, path::PathBuf, time::Duration};
use zbus::{fdo, zvariant::OwnedValue};

struct Application {
    marker: PathBuf,
    fail: bool,
    delay: Duration,
}
#[zbus::interface(name = "org.freedesktop.Application")]
impl Application {
    async fn activate(&self, _platform_data: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        println!("APPLICATION_ACTIVATE_STARTED");
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(fdo::Error::NotSupported(
                "Deliberate activation failure".into(),
            ));
        }
        std::fs::write(&self.marker, "ACTIVATED\n")
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let name = args
        .next()
        .ok_or("application_probe NAME MARKER [--fail | --slow]")?;
    let marker = args.next().ok_or("missing marker path")?.into();
    let mode = args.next().unwrap_or_default();
    if args.next().is_some() || !matches!(mode.as_str(), "" | "--fail" | "--slow") {
        return Err("application_probe NAME MARKER [--fail | --slow]".into());
    }
    let path = format!("/{}", name.replace('.', "/").replace('-', "_"));
    let _connection = zbus::connection::Builder::session()?
        .name(name.as_str())?
        .serve_at(
            path.as_str(),
            Application {
                marker,
                fail: mode == "--fail",
                delay: if mode == "--slow" {
                    Duration::from_secs(5)
                } else {
                    Duration::ZERO
                },
            },
        )?
        .build()
        .await?;
    println!("APPLICATION_PROBE_READY");
    std::future::pending::<()>().await;
    Ok(())
}
