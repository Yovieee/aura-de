//! Aura's Wayland server with nested and optional experimental DRM backends.
mod backend;
mod capture;
mod decorations;
mod glass;
#[cfg(feature = "native")]
mod native;
#[cfg(test)]
mod native_render_tests;
mod runtime;
mod scene;

use aura_core::GlassQualityPreset;
use std::{env, error::Error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Winit,
    Drm,
}

impl Backend {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value {
            "winit" => Ok(Self::Winit),
            "drm" => Ok(Self::Drm),
            _ => Err(format!("unknown backend {value:?}; choose winit or drm").into()),
        }
    }
}

const HELP: &str = "aura-compositor [--backend winit|drm] [--socket NAME] [--smoke-test]\n\
Configure with AURA_BACKEND, AURA_OUTPUT_SCALE (integer), AURA_GLASS_QUALITY.\n\
winit runs inside an existing X11/Wayland desktop and accepts AURA_WINIT_RESOLUTION.\n\
Experimental drm requires a Linux seat, DRM/KMS hardware and a build with --features native.";

#[derive(Debug)]
pub(crate) struct Config {
    pub backend: Backend,
    pub socket: String,
    pub size: (i32, i32),
    pub scale: i32,
    pub quality: GlassQualityPreset,
    pub smoke: bool,
}

fn parse_config(
    args: impl IntoIterator<Item = String>,
    environment: impl Fn(&str) -> Option<String>,
    native_available: bool,
) -> Result<Option<Config>, Box<dyn Error>> {
    let mut backend = environment("AURA_BACKEND").unwrap_or_else(|| "winit".into());
    let mut config = Config {
        backend: Backend::Winit,
        socket: environment("AURA_WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-aura-0".into()),
        size: (1280, 800),
        scale: environment("AURA_OUTPUT_SCALE")
            .unwrap_or_else(|| "1".into())
            .parse()?,
        quality: match environment("AURA_GLASS_QUALITY")
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
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => backend = args.next().ok_or("--backend requires a value")?,
            "--socket" => config.socket = args.next().ok_or("--socket requires a value")?,
            "--smoke-test" => config.smoke = true,
            "--help" | "-h" => return Ok(None),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    config.backend = Backend::parse(&backend)?;
    if config.backend == Backend::Drm && !native_available {
        return Err(
            "the drm backend requires an aura-compositor build with --features native".into(),
        );
    }
    if config.backend == Backend::Winit
        && let Some(size) = environment("AURA_WINIT_RESOLUTION")
    {
        let (width, height) = size
            .split_once('x')
            .ok_or("resolution must be WIDTHxHEIGHT")?;
        config.size = (width.parse()?, height.parse()?);
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
    Ok(Some(config))
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let Some(config) = parse_config(
        env::args().skip(1),
        |name| env::var(name).ok(),
        cfg!(feature = "native"),
    )?
    else {
        println!("{HELP}");
        return Ok(());
    };
    runtime::run(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(
        args: &[&str],
        variables: &[(&str, &str)],
        native: bool,
    ) -> Result<Option<Config>, Box<dyn Error>> {
        parse_config(
            args.iter().map(|value| (*value).into()),
            |name| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).into())
            },
            native,
        )
    }

    #[test]
    fn nested_defaults_and_cli_backend_override_are_preserved() {
        let config = parse(&[], &[], false).unwrap().unwrap();
        assert_eq!(config.backend, Backend::Winit);
        assert_eq!(config.size, (1280, 800));
        let config = parse(
            &["--backend", "winit"],
            &[
                ("AURA_BACKEND", "drm"),
                ("AURA_WINIT_RESOLUTION", "1024x640"),
            ],
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.backend, Backend::Winit);
        assert_eq!(config.size, (1024, 640));
    }

    #[test]
    fn native_ignores_nested_resolution_and_requires_native_build() {
        let variables = [
            ("AURA_BACKEND", "drm"),
            ("AURA_WINIT_RESOLUTION", "not-a-nested-size"),
        ];
        let config = parse(&[], &variables, true).unwrap().unwrap();
        assert_eq!(config.backend, Backend::Drm);
        assert!(
            parse(&[], &variables, false)
                .unwrap_err()
                .to_string()
                .contains("--features native")
        );
        assert!(parse(&["--backend", "winit"], &variables, true).is_err());
    }

    #[test]
    fn malformed_cli_and_unknown_backends_fail_before_startup() {
        for args in [
            vec!["--backend", "other"],
            vec!["--backend"],
            vec!["--socket"],
            vec!["--socket", "../outside"],
            vec!["--unknown"],
        ] {
            assert!(parse(&args, &[], true).is_err(), "{args:?}");
        }
        assert!(parse(&["--help"], &[], false).unwrap().is_none());
    }
}
