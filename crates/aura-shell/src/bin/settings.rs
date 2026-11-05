//! Session-scoped desktop controls backed by the running shell/compositor.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => aura_shell::settings::run(),
        [flag] if flag == "--smoke-test" => aura_shell::settings::run(),
        [flag] if flag == "--help" || flag == "-h" => {
            println!(
                "aura-settings [--smoke-test]\nSession-scoped window layout, glass quality and Do Not Disturb controls.\nRun inside an Aura session with a reachable org.aura.Shell1 service."
            );
            Ok(())
        }
        _ => Err("Usage: aura-settings [--smoke-test]".into()),
    }
}
