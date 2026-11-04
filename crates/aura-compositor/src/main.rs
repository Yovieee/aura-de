//! Aura's nested Wayland server. Native hardware backends remain explicitly unsupported.
mod glass;
mod runtime;

use aura_core::GlassQualityPreset;
use std::{env, error::Error};

#[derive(Debug)]
pub(crate) struct Config {
    pub socket: String,
    pub size: (i32, i32),
    pub scale: i32,
    pub quality: GlassQualityPreset,
    pub smoke: bool,
}

fn config() -> Result<Config, Box<dyn Error>> {
    let mut backend = env::var("AURA_BACKEND").unwrap_or_else(|_| "winit".into());
    let mut config = Config {
        socket: env::var("AURA_WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-aura-0".into()),
        size: (1280, 800),
        scale: env::var("AURA_OUTPUT_SCALE")
            .unwrap_or_else(|_| "1".into())
            .parse()?,
        quality: match env::var("AURA_GLASS_QUALITY")
            .as_deref()
            .unwrap_or("balanced")
        {
            "ultra" => GlassQualityPreset::Ultra,
            "balanced" => GlassQualityPreset::Balanced,
            "reduced-transparency" => GlassQualityPreset::ReducedTransparency,
            other => return Err(format!("invalid AURA_GLASS_QUALITY: {other}").into()),
        },
        smoke: false,
    };
    if let Ok(size) = env::var("AURA_WINIT_RESOLUTION") {
        let (width, height) = size
            .split_once('x')
            .ok_or("resolution must be WIDTHxHEIGHT")?;
        config.size = (width.parse()?, height.parse()?);
    }
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => backend = args.next().ok_or("--backend requires a value")?,
            "--socket" => config.socket = args.next().ok_or("--socket requires a value")?,
            "--smoke-test" => config.smoke = true,
            "--help" | "-h" => {
                println!(
                    "aura-compositor [--backend winit] [--socket NAME] [--smoke-test]\nConfigure with AURA_WINIT_RESOLUTION, AURA_OUTPUT_SCALE (integer), AURA_GLASS_QUALITY.\nRun inside an existing X11/Wayland desktop. Native DRM/KMS is not implemented."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    if backend != "winit" {
        return Err(format!("backend {backend:?} is not implemented; use winit").into());
    }
    if config.size.0 < 64 || config.size.1 < 64 || config.size.0 > 8192 || config.size.1 > 8192 {
        return Err("resolution must be between 64 and 8192 pixels per axis".into());
    }
    if !(1..=4).contains(&config.scale) {
        return Err("AURA_OUTPUT_SCALE must be an integer from 1 to 4".into());
    }
    if config.socket.is_empty() || config.socket.contains('/') {
        return Err("--socket must be a filename within XDG_RUNTIME_DIR".into());
    }
    Ok(config)
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    runtime::run(config()?)
}
