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
#[cfg(feature = "xwayland")]
mod xwayland;

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

const HELP: &str = "aura-compositor [--backend winit|drm] [--socket NAME] [--smoke-test] [--xwayland]\n\
Configure with AURA_BACKEND, AURA_OUTPUT_SCALE (integer), AURA_GLASS_QUALITY.\n\
winit runs inside an existing X11/Wayland desktop and accepts AURA_WINIT_RESOLUTION.\n\
Experimental drm requires a Linux seat, DRM/KMS hardware and a build with --features native.\n\
X11 applications require --xwayland (or AURA_XWAYLAND=1), Xwayland installed and a build with --features xwayland.\n\
Use --no-xwayland or AURA_XWAYLAND=0 to disable it.";

#[derive(Debug)]
pub(crate) struct Config {
    pub backend: Backend,
    pub socket: String,
    pub size: (i32, i32),
    pub scale: i32,
    pub quality: GlassQualityPreset,
    pub smoke: bool,
    pub xwayland: bool,
}

fn parse_config(
    args: impl IntoIterator<Item = String>,
    environment: impl Fn(&str) -> Option<String>,
    native_available: bool,
    xwayland_available: bool,
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
        xwayland: match environment("AURA_XWAYLAND").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(_) => return Err("AURA_XWAYLAND must be 0 or 1".into()),
        },
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => backend = args.next().ok_or("--backend requires a value")?,
            "--socket" => config.socket = args.next().ok_or("--socket requires a value")?,
            "--smoke-test" => config.smoke = true,
            "--xwayland" => config.xwayland = true,
            "--no-xwayland" => config.xwayland = false,
            "--help" | "-h" => return Ok(None),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    config.backend = Backend::parse(&backend)?;
    if config.xwayland && !xwayland_available {
        return Err("XWayland requires an aura-compositor build with --features xwayland".into());
    }
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
        cfg!(feature = "xwayland"),
    )?
    else {
        println!("{HELP}");
        return Ok(());
    };
    if config.xwayland {
        // X11's filesystem listener is bound before spawning the server. Set
        // privacy before any compositor threads exist, including under umask 0.
        // Shell/application processes are supervisor siblings and keep their mask.
        use smithay::reexports::rustix::{fs::Mode, process::umask};
        let private = Mode::from_raw_mode(0o077);
        let inherited = umask(private);
        umask(inherited | private);
    }
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
        parse_available(args, variables, native, false)
    }

    fn parse_available(
        args: &[&str],
        variables: &[(&str, &str)],
        native: bool,
        xwayland: bool,
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
            xwayland,
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

    #[test]
    fn xwayland_is_opt_in_and_requires_a_build_with_the_feature() {
        assert!(!parse(&[], &[], false).unwrap().unwrap().xwayland);
        for (args, variables) in [
            (vec!["--xwayland"], vec![]),
            (vec![], vec![("AURA_XWAYLAND", "1")]),
        ] {
            assert!(
                parse(&args, &variables, false)
                    .unwrap_err()
                    .to_string()
                    .contains("--features xwayland")
            );
            assert!(
                parse_available(&args, &variables, false, true)
                    .unwrap()
                    .unwrap()
                    .xwayland
            );
        }
        assert!(
            !parse(&["--no-xwayland"], &[("AURA_XWAYLAND", "1")], false)
                .unwrap()
                .unwrap()
                .xwayland
        );
        assert!(parse(&[], &[("AURA_XWAYLAND", "yes")], false).is_err());
    }
}
