//! Versioned user preferences. Call file operations from an I/O worker, never a UI callback.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_PREFERENCES_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub version: u32,
    pub layout: String,
    pub glass_quality: String,
    pub do_not_disturb: bool,
}
impl Preferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(
                "Unsupported preferences version; preserve or repair the existing file".into(),
            );
        }
        if !matches!(
            self.layout.as_str(),
            "floating" | "stage-manager" | "tiling"
        ) {
            return Err("Unknown saved window layout".into());
        }
        if !matches!(
            self.glass_quality.as_str(),
            "ultra" | "balanced" | "reduced-transparency"
        ) {
            return Err("Unknown saved glass quality".into());
        }
        Ok(())
    }

    pub fn from_status(status: &str, do_not_disturb: bool) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Status {
            layout: String,
            quality: String,
        }
        let status: Status = serde_json::from_str(status).map_err(|error| error.to_string())?;
        let preferences = Self {
            version: 1,
            layout: status.layout,
            glass_quality: status.quality,
            do_not_disturb,
        };
        preferences.validate()?;
        Ok(preferences)
    }
}

#[derive(Clone, Debug)]
pub struct PreferenceStore {
    path: PathBuf,
}
impl PreferenceStore {
    pub fn from_environment() -> Result<Self, String> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .ok_or("XDG_CONFIG_HOME or HOME is required to save preferences")?;
        if !config.is_absolute() {
            return Err("The preferences configuration directory must be an absolute path".into());
        }
        Ok(Self::at(config.join("aura/preferences.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Preferences>, String> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Could not inspect saved preferences: {error}")),
        };
        if !metadata.is_file() {
            return Err("Saved preferences must be a regular file".into());
        }
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Could not read saved preferences: {error}")),
        };
        if !file
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("Saved preferences must be a regular file".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_PREFERENCES_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Could not read saved preferences: {error}"))?;
        if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
            return Err("Saved preferences exceed the size limit; existing file preserved".into());
        }
        let preferences: Preferences = serde_json::from_slice(&bytes).map_err(|error| {
            format!("Saved preferences are invalid; repair or remove the existing file: {error}")
        })?;
        preferences.validate()?;
        Ok(Some(preferences))
    }

    /// Replace a valid previous snapshot atomically. Invalid/future files are never overwritten.
    pub fn save(&self, preferences: &Preferences) -> Result<(), String> {
        preferences.validate()?;
        self.load()?;
        let parent = self
            .path
            .parent()
            .ok_or("Preferences path has no parent directory")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| format!("Could not create the preferences directory: {error}"))?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let (mut file, path) = loop {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".preferences-{}-{sequence}.tmp",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => break (file, path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("Could not create a preferences snapshot: {error}"));
                }
            }
        };
        struct Temporary(PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Temporary(path);
        let mut bytes =
            serde_json::to_vec_pretty(preferences).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("Could not write the preferences snapshot: {error}"))?;
        drop(file);
        fs::rename(&temporary.0, &self.path)
            .map_err(|error| format!("Could not replace saved preferences: {error}"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not confirm saved preferences durability: {error}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "aura-preferences-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> PreferenceStore {
            PreferenceStore::at(self.0.join("aura/preferences.json"))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn profile() -> Preferences {
        Preferences {
            version: 1,
            layout: "tiling".into(),
            glass_quality: "ultra".into(),
            do_not_disturb: true,
        }
    }

    #[test]
    fn saves_private_complete_snapshots_and_reloads_replacements() {
        let root = Directory::new();
        let store = root.store();
        assert_eq!(store.load().unwrap(), None);
        store.save(&profile()).unwrap();
        assert_eq!(store.load().unwrap(), Some(profile()));
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut updated = profile();
        updated.layout = "floating".into();
        updated.do_not_disturb = false;
        store.save(&updated).unwrap();
        assert_eq!(store.load().unwrap(), Some(updated));
        assert_eq!(
            fs::read_dir(store.path().parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }
    #[test]
    fn malformed_future_and_oversized_files_are_preserved() {
        let root = Directory::new();
        let store = root.store();
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        for contents in [b"broken JSON".to_vec(), br#"{"version":2,"layout":"tiling","glass_quality":"ultra","do_not_disturb":true}"#.to_vec(),
            vec![b' '; MAX_PREFERENCES_BYTES as usize + 1],
            br#"{"version":1,"layout":"tiling","glass_quality":"ultra","do_not_disturb":true,"unknown":1}"#.to_vec()] {
            fs::write(store.path(), &contents).unwrap();
            assert!(store.load().is_err()); assert!(store.save(&profile()).is_err());
            assert_eq!(fs::read(store.path()).unwrap(), contents);
        }
    }
    #[test]
    fn invalid_candidates_and_unwritable_target_do_not_report_success() {
        let root = Directory::new();
        let store = root.store();
        store.save(&profile()).unwrap();
        let original = fs::read(store.path()).unwrap();
        let mut invalid = profile();
        invalid.glass_quality = "unsupported".into();
        assert!(store.save(&invalid).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), original);
        fs::remove_file(store.path()).unwrap();
        fs::create_dir(store.path()).unwrap();
        assert!(store.save(&profile()).is_err());
        assert!(store.path().is_dir());
    }
    #[test]
    fn symbolic_link_snapshots_are_rejected_without_modifying_the_target() {
        let root = Directory::new();
        let store = root.store();
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        let target = root.0.join("external.json");
        fs::write(&target, b"keep this content").unwrap();
        std::os::unix::fs::symlink(&target, store.path()).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&profile()).is_err());
        assert!(
            fs::symlink_metadata(store.path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).unwrap(), b"keep this content");
    }
    #[test]
    fn readback_requires_known_complete_actual_values() {
        assert!(Preferences::from_status(r#"{"layout":"tiling"}"#, false).is_err());
        assert!(
            Preferences::from_status(r#"{"layout":"unknown","quality":"balanced"}"#, false)
                .is_err()
        );
        assert_eq!(
            Preferences::from_status(r#"{"layout":"tiling","quality":"ultra","other":4}"#, true)
                .unwrap(),
            profile()
        );
    }
}
