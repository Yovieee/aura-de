//! Session entry point. A compositor, its shell, and their cleanup have one owner.

mod portal;
mod scanout;
mod supervisor;

use std::os::unix::process::CommandExt;
use supervisor::{Options, ParseResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match Options::parse(std::env::args().skip(1))? {
        ParseResult::Help => {
            println!("{}", supervisor::HELP);
            return Ok(());
        }
        ParseResult::Run(options) => options,
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Replace this process before starting Tokio threads; do not set global
    // environment variables from a runtime worker.
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        let error = std::process::Command::new("dbus-run-session")
            .arg("--")
            .arg(std::env::current_exe()?)
            .args(std::env::args_os().skip(1))
            .exec();
        return Err(format!("could not start session D-Bus with dbus-run-session: {error}").into());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        if options.portal_only {
            let _service = portal::serve().await?;
            tracing::info!("Aura portal backend exported; screen capture is not supported yet");
            supervisor::shutdown_signal().await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        } else {
            supervisor::run(options).await?;
            Ok(())
        }
    })
}
