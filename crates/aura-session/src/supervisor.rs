use std::{
    ffi::OsString,
    io::{self, Read},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::ExitStatus,
    time::Duration,
};

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};
use tracing::{info, warn};

pub const HELP: &str = "Aura session supervisor\n\
Usage: aura-session [--backend winit|drm] [--socket NAME] [--compositor PATH] [--shell PATH]\n\
                    [--no-shell] [--no-portal] [--xwayland] [--startup-timeout SECONDS]\n\
       aura-session --portal\n\
\nStarts a compositor, waits for its Wayland socket, then starts the shell.\n\
SIGINT/SIGTERM or a child exiting stops and reaps the session's process groups.\n\
If there is no session bus, dbus-run-session creates one.\n\
The default winit backend uses an existing graphical desktop. Experimental drm\n\
requires a compositor built with --features native and an accessible Linux seat.\n\
Optional --xwayland (or AURA_XWAYLAND=1) requires an XWayland-enabled compositor.\n\
The shell starts after its X11 display is ready; --no-xwayland disables it.\n\
ScreenCast requires this session's authenticated capture source, a running PipeWire daemon,\n\
and explicit approval in the screen-sharing consent overlay. Standalone --portal\n\
does not inherit screen capture capabilities.";

#[derive(Debug)]
pub struct Options {
    pub portal_only: bool,
    pub backend: String,
    pub socket: String,
    pub compositor: OsString,
    pub shell: Option<OsString>,
    pub portal: bool,
    pub startup_timeout: Duration,
    pub xwayland: bool,
}

pub enum ParseResult {
    Help,
    Run(Options),
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> io::Result<ParseResult> {
        let mut args = args.into_iter();
        let mut options = Self {
            portal_only: false,
            backend: std::env::var("AURA_BACKEND").unwrap_or_else(|_| "winit".into()),
            socket: std::env::var("AURA_WAYLAND_DISPLAY")
                .unwrap_or_else(|_| "wayland-aura-0".into()),
            compositor: sibling_or_path("aura-compositor"),
            shell: Some(sibling_or_path("aura-shell")),
            portal: true,
            startup_timeout: Duration::from_secs(15),
            xwayland: match std::env::var("AURA_XWAYLAND").as_deref() {
                Err(std::env::VarError::NotPresent) | Ok("0") => false,
                Ok("1") => true,
                _ => return Err(invalid("AURA_XWAYLAND must be 0 or 1")),
            },
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => return Ok(ParseResult::Help),
                "--portal" => options.portal_only = true,
                "--backend" => options.backend = value(&mut args, &arg)?,
                "--socket" => options.socket = value(&mut args, &arg)?,
                "--compositor" => options.compositor = value(&mut args, &arg)?.into(),
                "--shell" => options.shell = Some(value(&mut args, &arg)?.into()),
                "--no-shell" => options.shell = None,
                "--no-portal" => options.portal = false,
                "--xwayland" => options.xwayland = true,
                "--no-xwayland" => options.xwayland = false,
                "--startup-timeout" => {
                    let seconds: u64 = value(&mut args, &arg)?.parse().map_err(|_| {
                        invalid("startup timeout must be an integer between 1 and 300")
                    })?;
                    if !(1..=300).contains(&seconds) {
                        return Err(invalid("startup timeout must be between 1 and 300 seconds"));
                    }
                    options.startup_timeout = Duration::from_secs(seconds);
                }
                _ => return Err(invalid(format!("unknown argument {arg}; use --help"))),
            }
        }
        if !options.portal_only {
            validate_socket_name(&options.socket)?;
        }
        if !options.portal_only && !matches!(options.backend.as_str(), "winit" | "drm") {
            return Err(invalid("unknown backend; choose winit or drm"));
        }
        Ok(ParseResult::Run(options))
    }
}

fn value(args: &mut impl Iterator<Item = String>, argument: &str) -> io::Result<String> {
    args.next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("{argument} requires a value")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn sibling_or_path(name: &str) -> OsString {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|p| p.join(name)))
        .filter(|path| path.is_file())
        .map(PathBuf::into_os_string)
        .unwrap_or_else(|| name.into())
}

fn validate_socket_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 80
        || name == "."
        || name == ".."
        || name
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
    {
        Err(invalid(
            "Wayland socket must be a simple name of at most 80 ASCII letters, digits, '.', '_' or '-'",
        ))
    } else {
        Ok(())
    }
}

/// Capabilities are generated in memory for this session and passed only to its
/// compositor. They are never exported to the shell, applications or D-Bus
/// activation environment, and must never be included in diagnostics.
fn capture_config(runtime: &Path) -> io::Result<crate::capture::CaptureConfig> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(crate::capture::CaptureConfig {
        socket: runtime.join("aura-capture.sock"),
        token,
        remote: std::env::var("PIPEWIRE_REMOTE").ok(),
    })
}

struct RuntimeDirectory {
    path: PathBuf,
    owned: bool,
}

impl RuntimeDirectory {
    fn prepare(existing: Option<OsString>) -> io::Result<Self> {
        if let Some(existing) = existing {
            let path = PathBuf::from(existing);
            if !path.is_absolute() || !path.is_dir() {
                return Err(invalid(
                    "XDG_RUNTIME_DIR must be an existing absolute directory",
                ));
            }
            let metadata = std::fs::metadata(&path)?;
            if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0
            {
                return Err(invalid(
                    "XDG_RUNTIME_DIR must be owned by this user and private (mode 0700)",
                ));
            }
            return Ok(Self { path, owned: false });
        }
        let path = std::env::temp_dir().join(format!("aura-session-{}", std::process::id()));
        std::fs::create_dir(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self { path, owned: true })
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        if self.owned
            && let Err(error) = std::fs::remove_dir_all(&self.path)
        {
            warn!(%error, "could not remove private session runtime directory");
        }
    }
}

struct ManagedChild {
    name: &'static str,
    child: Child,
    group: Pid,
}

impl ManagedChild {
    fn spawn(name: &'static str, command: &mut Command) -> io::Result<Self> {
        command.process_group(0).kill_on_drop(true);
        let child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not launch {name}: {error}; build/install all workspace binaries first"
                ),
            )
        })?;
        let group = Pid::from_raw(child.id().expect("new child has process ID") as i32)
            .expect("positive child ID");
        Ok(Self { name, child, group })
    }

    async fn stop(&mut self) {
        // Each child owns a process group, including any helper descendants.
        let _ = kill_process_group(self.group, Signal::TERM);
        if timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            warn!(
                child = self.name,
                "child did not stop after SIGTERM; forcing shutdown"
            );
            let _ = kill_process_group(self.group, Signal::KILL);
            let _ = self.child.wait().await;
        }
        // A direct child can exit while helpers ignore SIGTERM. Group ownership
        // remains with its descendants until its final member exits.
        let _ = kill_process_group(self.group, Signal::KILL);
    }
}

async fn wait_for_socket(
    child: &mut ManagedChild,
    socket: &Path,
    startup_timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + startup_timeout;
    loop {
        if let Some(status) = child.child.try_wait()? {
            return Err(io::Error::other(format!(
                "{} exited before Wayland was ready: {status}",
                child.name
            )));
        }
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Wayland socket {} was not ready within {}s",
                    socket.display(),
                    startup_timeout.as_secs()
                ),
            ));
        }
        sleep(Duration::from_millis(30)).await;
    }
}

#[derive(serde::Deserialize)]
struct XwaylandStatus {
    xwayland_ready: bool,
    xwayland_display: Option<String>,
}

fn xwayland_display(response: &str) -> io::Result<Option<String>> {
    let status: XwaylandStatus = serde_json::from_str(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !status.xwayland_ready {
        return Ok(None);
    }
    let display = status.xwayland_display.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "XWayland is ready without a display",
        )
    })?;
    if !display.strip_prefix(':').is_some_and(|number| {
        !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && number.parse::<u32>().is_ok()
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the compositor returned an invalid local XWayland display",
        ));
    }
    Ok(Some(display))
}

async fn query_xwayland(control: &Path, deadline: Duration) -> io::Result<Option<String>> {
    timeout(deadline, async {
        let mut stream = UnixStream::connect(control).await?;
        stream.write_all(b"status\n").await?;
        let mut response = String::new();
        BufReader::new(stream)
            .take(64 * 1024 + 1)
            .read_line(&mut response)
            .await?;
        if response.len() > 64 * 1024 || !response.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the compositor did not return a bounded, complete status response",
            ));
        }
        xwayland_display(&response)
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "the compositor XWayland status query timed out",
        )
    })?
}

async fn wait_for_xwayland(
    child: &mut ManagedChild,
    control: &Path,
    startup_timeout: Duration,
) -> io::Result<String> {
    let deadline = Instant::now() + startup_timeout;
    loop {
        if let Some(status) = child.child.try_wait()? {
            return Err(io::Error::other(format!(
                "{} exited before XWayland was ready: {status}",
                child.name
            )));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "XWayland was not ready within {}s",
                    startup_timeout.as_secs_f32()
                ),
            ));
        }
        match query_xwayland(control, remaining.min(Duration::from_millis(250))).await {
            Ok(Some(display)) => return Ok(display),
            Ok(None) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        sleep(Duration::from_millis(30).min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

struct Shutdown {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl Shutdown {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! { _ = self.interrupt.recv() => {}, _ = self.terminate.recv() => {} }
    }
}

pub async fn shutdown_signal() -> io::Result<()> {
    Shutdown::new()?.wait().await;
    Ok(())
}

pub async fn run(options: Options) -> io::Result<()> {
    // Install handlers before children start, and retain receivers between
    // startup and runtime so a signal in the transition cannot be lost.
    let mut shutdown = Shutdown::new()?;
    let runtime = RuntimeDirectory::prepare(std::env::var_os("XDG_RUNTIME_DIR"))?;
    let socket = runtime.path.join(&options.socket);
    let control = std::env::var_os("AURA_CONTROL_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime.path.join("aura-control.sock"));
    if std::fs::symlink_metadata(&socket).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to use an existing Wayland socket: {}",
                socket.display()
            ),
        ));
    }
    let capture = options
        .portal
        .then(|| capture_config(&runtime.path))
        .transpose()?;
    let mut compositor_command = Command::new(&options.compositor);
    compositor_command
        .args(["--backend", &options.backend, "--socket", &options.socket])
        .env("XDG_RUNTIME_DIR", &runtime.path)
        .env("AURA_CONTROL_SOCKET", &control)
        .env("AURA_WAYLAND_DISPLAY", &options.socket)
        .env("XDG_CURRENT_DESKTOP", "Aura");
    compositor_command.arg(if options.xwayland {
        "--xwayland"
    } else {
        "--no-xwayland"
    });
    if let Some(capture) = &capture {
        compositor_command
            .env("AURA_CAPTURE_SOCKET", &capture.socket)
            .env("AURA_CAPTURE_TOKEN", &capture.token)
            .env("AURA_CAPTURE_OWNER_PID", std::process::id().to_string());
    } else {
        compositor_command
            .env_remove("AURA_CAPTURE_SOCKET")
            .env_remove("AURA_CAPTURE_TOKEN")
            .env_remove("AURA_CAPTURE_OWNER_PID");
    }
    // The native presenter obtains its mode from DRM and never needs a nested
    // host display. Clients below always receive the new Wayland socket.
    if options.backend == "drm" {
        compositor_command
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("AURA_WINIT_RESOLUTION");
    }
    let mut compositor = ManagedChild::spawn("aura-compositor", &mut compositor_command)?;
    let ready = tokio::select! {
        result = wait_for_socket(&mut compositor, &socket, options.startup_timeout) => result,
        () = shutdown.wait() => { compositor.stop().await; return Ok(()); }
    };
    if let Err(error) = ready {
        compositor.stop().await;
        return Err(error);
    }
    info!(socket = %socket.display(), "Wayland compositor is accepting connections");

    let x_display = if options.xwayland {
        let ready = tokio::select! {
            result = wait_for_xwayland(&mut compositor, &control, options.startup_timeout) => result,
            () = shutdown.wait() => { compositor.stop().await; return Ok(()); }
        };
        match ready {
            Ok(x_display_name) => {
                info!(x_display = %x_display_name, "XWayland window manager is ready");
                Some(x_display_name)
            }
            Err(error) => {
                compositor.stop().await;
                return Err(error);
            }
        }
    } else {
        None
    };

    let mut shell = if let Some(program) = options.shell {
        let mut command = Command::new(program);
        command
            .env("XDG_RUNTIME_DIR", &runtime.path)
            .env("WAYLAND_DISPLAY", &options.socket)
            .env("AURA_WAYLAND_DISPLAY", &options.socket)
            .env("AURA_CONTROL_SOCKET", &control)
            .env("XDG_CURRENT_DESKTOP", "Aura")
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove("AURA_CAPTURE_SOCKET")
            .env_remove("AURA_CAPTURE_TOKEN")
            .env_remove("AURA_CAPTURE_OWNER_PID");
        if let Some(display) = &x_display {
            command.env("DISPLAY", display).env_remove("XAUTHORITY");
        }
        match ManagedChild::spawn("aura-shell", &mut command) {
            Ok(child) => Some(child),
            Err(error) => {
                compositor.stop().await;
                return Err(error);
            }
        }
    } else {
        None
    };

    let _portal = if let Some(capture) = capture {
        let consent = crate::portal::ConsentUi {
            program: sibling_or_path("aura-launcher"),
            wayland_display: options.socket.clone().into(),
            runtime_dir: runtime.path.clone(),
        };
        let result = tokio::select! {
            service = crate::portal::serve_with_capture(capture, consent) => service,
            () = shutdown.wait() => {
                if let Some(shell) = &mut shell { shell.stop().await; }
                compositor.stop().await;
                return Ok(());
            }
        };
        match result {
            Ok(service) => Some(service),
            Err(error) => {
                warn!(%error, "portal backend could not be exported; desktop continues without ScreenCast");
                None
            }
        }
    } else {
        None
    };

    let outcome = tokio::select! {
        () = shutdown.wait() => Ok(()),
        result = compositor.child.wait() => child_exit("compositor", result),
        result = async {
            match shell.as_mut() {
                Some(shell) => shell.child.wait().await,
                None => std::future::pending::<io::Result<ExitStatus>>().await,
            }
        } => result.and_then(|status| Err(io::Error::other(format!("shell exited; ending session: {status}")))),
    };
    if let Some(shell) = &mut shell {
        shell.stop().await;
    }
    compositor.stop().await;
    info!("Aura session processes stopped and reaped");
    outcome
}

fn child_exit(name: &str, result: io::Result<ExitStatus>) -> io::Result<()> {
    let status = result?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{name} exited: {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    fn parse(args: &[&str]) -> io::Result<ParseResult> {
        Options::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn capture_capabilities_are_unique_and_not_debug_logged() {
        let first = capture_config(Path::new("/tmp/aura-capability-test")).unwrap();
        let second = capture_config(Path::new("/tmp/aura-capability-test")).unwrap();
        assert_eq!(first.token.len(), 64);
        assert!(first.token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(
            first.token != second.token,
            "sessions must not reuse capture capabilities"
        );
        assert!(
            !format!("{first:?}").contains(&first.token),
            "capability must be redacted from Debug"
        );
    }

    #[test]
    fn capture_session_rejects_a_shared_runtime_directory() {
        let path = std::env::temp_dir().join(format!(
            "aura-runtime-permission-test-{}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RuntimeDirectory::prepare(Some(path.clone().into())).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let prepared = RuntimeDirectory::prepare(Some(path.clone().into())).unwrap();
        drop(prepared);
        assert!(
            path.exists(),
            "caller-owned runtime directories must be preserved"
        );
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn rejects_invalid_socket_backend_and_timeout() {
        for args in [
            vec!["--socket", "../../socket"],
            vec!["--backend", "tty-udev"],
            vec!["--startup-timeout", "0"],
            vec!["--shell"],
            vec!["--unexpected"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(matches!(parse(&["--help"]).unwrap(), ParseResult::Help));
        let ParseResult::Run(options) =
            parse(&["--socket", "wayland-test-1", "--no-shell", "--no-portal"]).unwrap()
        else {
            panic!("run expected")
        };
        assert_eq!(options.socket, "wayland-test-1");
        assert!(options.shell.is_none());
        assert!(!options.portal);
        let ParseResult::Run(options) = parse(&["--backend", "drm"]).unwrap() else {
            panic!("run expected")
        };
        assert_eq!(options.backend, "drm");
        let ParseResult::Run(options) = parse(&["--xwayland"]).unwrap() else {
            panic!("run expected")
        };
        assert!(options.xwayland);
        let ParseResult::Run(options) = parse(&["--xwayland", "--no-xwayland"]).unwrap() else {
            panic!("run expected")
        };
        assert!(!options.xwayland);
    }

    #[test]
    fn readiness_requires_the_ready_flag_and_a_local_display() {
        assert_eq!(
            xwayland_display(r#"{"xwayland_ready":false,"xwayland_display":":9"}"#).unwrap(),
            None
        );
        assert_eq!(
            xwayland_display(r#"{"xwayland_ready":true,"xwayland_display":":9"}"#).unwrap(),
            Some(":9".into())
        );
        for response in [
            r#"{"xwayland_ready":true,"xwayland_display":null}"#,
            r#"{"xwayland_ready":true,"xwayland_display":"host:9"}"#,
            r#"{"xwayland_ready":true,"xwayland_display":":"}"#,
            r#"{"xwayland_ready":true,"xwayland_display":":1.0"}"#,
            r#"{"xwayland_ready":true,"xwayland_display":":4294967296"}"#,
            "not json",
        ] {
            assert_eq!(
                xwayland_display(response).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[tokio::test]
    async fn xwayland_readiness_waits_for_the_window_manager_status() {
        let root =
            std::env::temp_dir().join(format!("aura-xwayland-ready-test-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let control = root.join("control.sock");
        let listener = UnixListener::bind(&control).unwrap();
        let responder = tokio::spawn(async move {
            for ready in [false, true] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                stream.read_line(&mut request).await.unwrap();
                assert_eq!(request, "status\n");
                let response =
                    format!("{{\"xwayland_ready\":{ready},\"xwayland_display\":\":19\"}}\n");
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        let mut child = ManagedChild::spawn("test", &mut command).unwrap();
        let result = wait_for_xwayland(&mut child, &control, Duration::from_secs(1)).await;
        child.stop().await;
        responder.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(result.unwrap(), ":19");
    }

    #[tokio::test]
    async fn silent_xwayland_status_respects_the_startup_deadline() {
        let root =
            std::env::temp_dir().join(format!("aura-xwayland-silent-test-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let control = root.join("control.sock");
        let listener = UnixListener::bind(&control).unwrap();
        let responder = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        let mut child = ManagedChild::spawn("test", &mut command).unwrap();
        let start = Instant::now();
        let result = wait_for_xwayland(&mut child, &control, Duration::from_millis(50)).await;
        child.stop().await;
        responder.abort();
        let _ = responder.await;
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn ready_requires_a_connectable_socket_and_cleanup_reaps_child() {
        let root =
            std::env::temp_dir().join(format!("aura-supervisor-test-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let socket = root.join("wayland-test");
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        let mut child = ManagedChild::spawn("test", &mut command).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        wait_for_socket(&mut child, &socket, Duration::from_secs(1))
            .await
            .unwrap();
        child.stop().await;
        assert!(child.child.try_wait().unwrap().is_some());
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reports_early_process_failure_instead_of_readiness() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 23"]);
        let mut child = ManagedChild::spawn("failed-compositor", &mut command).unwrap();
        let error = wait_for_socket(
            &mut child,
            Path::new("/nonexistent/aura-socket"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exited before Wayland was ready")
        );
        child.stop().await;
    }

    #[tokio::test]
    async fn timeout_is_an_error_and_does_not_leave_process_alive() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        let mut child = ManagedChild::spawn("unready-compositor", &mut command).unwrap();
        let error = wait_for_socket(
            &mut child,
            Path::new("/nonexistent/aura-socket"),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        child.stop().await;
        assert!(child.child.try_wait().unwrap().is_some());
    }
}
