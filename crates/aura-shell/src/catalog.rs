//! Desktop-entry snapshots refreshed on a single background worker.
//!
//! UI callbacks only receive an already parsed snapshot. Directory traversal and
//! desktop-file parsing stay on the worker, including the initial scan.

use crate::desktop::{self, DesktopEntryItem};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub struct CatalogSnapshot {
    /// Zero until the initial background scan completes, then advances only
    /// when the parsed application list changes.
    pub revision: u64,
    pub entries: Arc<Vec<DesktopEntryItem>>,
}

pub struct DesktopCatalog {
    updates: mpsc::Receiver<CatalogSnapshot>,
    current: CatalogSnapshot,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl DesktopCatalog {
    /// Capture this process's XDG search directories and start asynchronous
    /// discovery. Changes are normally observed within 500 milliseconds.
    pub fn new() -> io::Result<Self> {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".local/share"))
            });
        let mut dirs: Vec<_> = data_home.into_iter().collect();
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        dirs.extend(std::env::split_paths(&data_dirs));
        Self::with_dirs(dirs, REFRESH_INTERVAL)
    }

    /// Use explicit data directories (each containing `applications/`) and a
    /// scan interval, without changing the process environment.
    pub fn with_dirs(dirs: Vec<PathBuf>, interval: Duration) -> io::Result<Self> {
        if interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "desktop catalog refresh interval must be positive",
            ));
        }
        let (publish, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("aura-desktop-catalog".into())
            .spawn(move || refresh(dirs, interval, publish, stopped))?;
        Ok(Self {
            updates,
            current: CatalogSnapshot::default(),
            stop,
            worker: Some(worker),
        })
    }

    /// Poll without waiting on filesystem work or a lock. There is at most one
    /// queued update; its successor is retained by the worker under backpressure.
    pub fn snapshot(&mut self) -> CatalogSnapshot {
        if let Ok(snapshot) = self.updates.try_recv() {
            self.current = snapshot;
        }
        self.current.clone()
    }
}

impl Drop for DesktopCatalog {
    fn drop(&mut self) {
        // Wake the worker immediately instead of waiting for its scan interval.
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("desktop-entry refresh worker stopped unexpectedly");
        }
    }
}

fn refresh(
    dirs: Vec<PathBuf>,
    interval: Duration,
    publish: mpsc::SyncSender<CatalogSnapshot>,
    stopped: mpsc::Receiver<()>,
) {
    let mut current = CatalogSnapshot::default();
    let mut pending = None;
    loop {
        match stopped.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let entries = desktop::discover_in(&dirs);
        if current.revision == 0 || *current.entries != entries {
            current = CatalogSnapshot {
                revision: current.revision.saturating_add(1),
                entries: Arc::new(entries),
            };
            pending = Some(current.clone());
        }
        if let Some(snapshot) = pending.take() {
            match publish.try_send(snapshot) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(snapshot)) => pending = Some(snapshot),
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
        match stopped.recv_timeout(interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "aura-catalog-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn data(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.join("applications")).unwrap();
            path
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(path: &std::path::Path, name: &str, program: &str) {
        fs::write(
            path,
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={program}\n"),
        )
        .unwrap();
    }
    fn until(
        catalog: &mut DesktopCatalog,
        predicate: impl Fn(&CatalogSnapshot) -> bool,
    ) -> CatalogSnapshot {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = catalog.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "catalog did not refresh");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn observes_install_edit_and_uninstall_without_changing_old_snapshots() {
        let root = Directory::new();
        let data = root.data("user");
        let path = data.join("applications/editor.desktop");
        let mut catalog = DesktopCatalog::with_dirs(vec![data], Duration::from_millis(20)).unwrap();
        let initial = until(&mut catalog, |snapshot| snapshot.revision != 0);
        assert!(initial.entries.is_empty());
        entry(&path, "Editor", "editor --first");
        let installed = until(&mut catalog, |snapshot| {
            snapshot
                .entries
                .first()
                .is_some_and(|entry| entry.name == "Editor")
        });
        entry(&path, "Updated Editor", "editor --second");
        let changed = until(&mut catalog, |snapshot| {
            snapshot.entries.first().is_some_and(|entry| {
                entry.name == "Updated Editor" && entry.exec == "editor --second"
            })
        });
        assert!(changed.revision > installed.revision);
        assert_eq!(installed.entries[0].name, "Editor");
        fs::remove_file(path).unwrap();
        let removed = until(&mut catalog, |snapshot| {
            snapshot.revision > changed.revision && snapshot.entries.is_empty()
        });
        thread::sleep(Duration::from_millis(80));
        assert_eq!(catalog.snapshot().revision, removed.revision);
    }

    #[test]
    fn user_overrides_and_hidden_tombstones_follow_live_precedence() {
        let root = Directory::new();
        let user = root.data("user");
        let system = root.data("system");
        let system_file = system.join("applications/editor.desktop");
        let user_file = user.join("applications/editor.desktop");
        entry(&system_file, "System Editor", "editor");
        let mut catalog =
            DesktopCatalog::with_dirs(vec![user, system], Duration::from_millis(20)).unwrap();
        until(&mut catalog, |snapshot| {
            snapshot
                .entries
                .first()
                .is_some_and(|entry| entry.name == "System Editor")
        });
        fs::write(&user_file, "[Desktop Entry]\nHidden=true\n").unwrap();
        until(&mut catalog, |snapshot| snapshot.entries.is_empty());
        entry(&user_file, "User Editor", "user-editor");
        until(&mut catalog, |snapshot| {
            snapshot
                .entries
                .first()
                .is_some_and(|entry| entry.name == "User Editor")
        });
        fs::remove_file(user_file).unwrap();
        until(&mut catalog, |snapshot| {
            snapshot
                .entries
                .first()
                .is_some_and(|entry| entry.name == "System Editor")
        });
        fs::remove_file(system_file).unwrap();
        until(&mut catalog, |snapshot| snapshot.entries.is_empty());
    }

    #[test]
    fn slow_subscriber_eventually_receives_the_latest_change() {
        let root = Directory::new();
        let data = root.data("user");
        let path = data.join("applications/editor.desktop");
        entry(&path, "First", "editor");
        let mut catalog = DesktopCatalog::with_dirs(vec![data], Duration::from_millis(20)).unwrap();
        // Leave the first channel slot occupied while the underlying file changes.
        thread::sleep(Duration::from_millis(100));
        entry(&path, "Latest", "editor --updated");
        thread::sleep(Duration::from_millis(100));
        until(&mut catalog, |snapshot| {
            snapshot
                .entries
                .first()
                .is_some_and(|entry| entry.name == "Latest")
        });
    }

    #[test]
    fn dropping_catalog_wakes_a_worker_waiting_for_the_next_scan() {
        let root = Directory::new();
        let mut catalog =
            DesktopCatalog::with_dirs(vec![root.data("user")], Duration::from_secs(30)).unwrap();
        until(&mut catalog, |snapshot| snapshot.revision != 0);
        let start = Instant::now();
        drop(catalog);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
