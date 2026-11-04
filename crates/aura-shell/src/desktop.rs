//! XDG desktop-entry discovery and shell-free Exec launching.
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
#[derive(Clone, Debug)]
pub struct DesktopEntryItem {
    pub app_id: String,
    pub name: String,
    pub keywords: Vec<String>,
    pub exec: String,
    pub icon: String,
    pub file: PathBuf,
    pub working_dir: Option<PathBuf>,
}

fn unescape(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => result.push(' '),
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some(v) => {
                    result.push('\\');
                    result.push(v);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}
fn parse_entry(contents: &str, id: String, file: PathBuf) -> Option<DesktopEntryItem> {
    let mut active = false;
    let mut values = BTreeMap::new();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        if line.starts_with('[') {
            active = line == "[Desktop Entry]";
            continue;
        }
        if active && let Some((k, v)) = line.split_once('=') {
            values.insert(k.trim(), v.trim());
        }
    }
    if values.get("Type") != Some(&"Application")
        || values.get("Hidden") == Some(&"true")
        || values.get("NoDisplay") == Some(&"true")
        || values.get("Terminal") == Some(&"true")
    {
        return None;
    }
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "Aura".into());
    let desktops: Vec<_> = current.split(':').collect();
    if values
        .get("OnlyShowIn")
        .is_some_and(|v| !v.split(';').any(|d| desktops.contains(&d)))
        || values
            .get("NotShowIn")
            .is_some_and(|v| v.split(';').any(|d| desktops.contains(&d)))
    {
        return None;
    }
    if let Some(program) = values.get("TryExec")
        && !executable(program)
    {
        return None;
    }
    let entry = DesktopEntryItem {
        app_id: id,
        name: unescape(values.get("Name")?),
        keywords: values
            .get("Keywords")
            .unwrap_or(&"")
            .split(';')
            .filter(|s| !s.is_empty())
            .map(unescape)
            .collect(),
        exec: values.get("Exec")?.to_string(),
        icon: unescape(values.get("Icon").unwrap_or(&"")),
        file,
        working_dir: values
            .get("Path")
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(unescape(s))),
    };
    exec_argv(&entry).ok()?;
    Some(entry)
}
fn executable(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let check = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return check(Path::new(program));
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|dir| check(&dir.join(program))))
}
fn files(dir: &Path, root: &Path, output: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for item in entries.flatten() {
        let path = item.path();
        let Ok(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            files(&path, root, output);
        } else if kind.is_file()
            && path.extension().is_some_and(|x| x == "desktop")
            && let Ok(relative) = path.strip_prefix(root)
        {
            output.push((relative.to_string_lossy().replace('/', "-"), path));
        }
    }
}
pub fn discover() -> Vec<DesktopEntryItem> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let mut dirs: Vec<_> = data_home.into_iter().collect();
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&data_dirs));
    discover_in(&dirs)
}
pub fn discover_in(dirs: &[PathBuf]) -> Vec<DesktopEntryItem> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for root in dirs.iter().map(|p| p.join("applications")) {
        let mut candidates = Vec::new();
        files(&root, &root, &mut candidates);
        candidates.sort();
        for (id, path) in candidates {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Ok(contents) = std::fs::read_to_string(&path)
                && let Some(entry) = parse_entry(&contents, id, path)
            {
                entries.push(entry);
            }
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}
/// No shell is involved. Unsupported or malformed field codes fail closed.
pub fn exec_argv(entry: &DesktopEntryItem) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = entry.exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' => {
                let next = chars.next().ok_or("Trailing Exec escape")?;
                word.push(next);
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("Unterminated Exec quote".into());
    }
    if started {
        tokens.push(word);
    }
    let mut args = Vec::new();
    for token in tokens {
        if ["%f", "%F", "%u", "%U"].contains(&token.as_str()) {
            continue;
        }
        if token == "%i" {
            if !entry.icon.is_empty() {
                args.extend(["--icon".into(), entry.icon.clone()]);
            }
            continue;
        }
        let mut expanded = String::new();
        let mut chars = token.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                match chars.next() {
                    Some('%') => expanded.push('%'),
                    Some('c') => expanded.push_str(&entry.name),
                    Some('k') => expanded.push_str(&entry.file.to_string_lossy()),
                    _ => return Err("Unsupported or embedded file Exec field code".into()),
                }
            } else {
                expanded.push(c);
            }
        }
        args.push(expanded);
    }
    if args.first().is_none_or(String::is_empty) {
        return Err("Missing Exec program".into());
    }
    Ok(args)
}
pub fn launch(entry: &DesktopEntryItem) -> Result<(), String> {
    let args = exec_argv(entry)?;
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = &entry.working_dir {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("{}: {e}", entry.name))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
pub fn search<'a>(entries: &'a [DesktopEntryItem], query: &str) -> Vec<&'a DesktopEntryItem> {
    let query = query.trim().to_lowercase();
    let mut scores: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.name.to_lowercase();
            let score = if query.is_empty() || name.starts_with(&query) {
                100
            } else if name.contains(&query) {
                75
            } else if entry
                .keywords
                .iter()
                .any(|k| k.to_lowercase().contains(&query))
            {
                50
            } else {
                let mut remaining = query.chars();
                let mut next = remaining.next();
                for c in name.chars() {
                    if Some(c) == next {
                        next = remaining.next();
                    }
                }
                if next.is_none() {
                    25
                } else {
                    return None;
                }
            };
            Some((score, entry))
        })
        .collect();
    scores.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scores.into_iter().map(|(_, e)| e).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn entry(exec: &str) -> DesktopEntryItem {
        DesktopEntryItem {
            app_id: "test.desktop".into(),
            name: "Test Application".into(),
            keywords: vec![],
            exec: exec.into(),
            icon: "test-icon".into(),
            file: PathBuf::from("/tmp/test.desktop"),
            working_dir: None,
        }
    }
    #[test]
    fn exec_quotes_fields_and_shell_literals() {
        let e = entry("app \"two words\" %c %i %f %% \"$(touch /tmp/nope)\"");
        assert_eq!(
            exec_argv(&e).unwrap(),
            vec![
                "app",
                "two words",
                "Test Application",
                "--icon",
                "test-icon",
                "%",
                "$(touch /tmp/nope)"
            ]
        );
        assert!(exec_argv(&entry("app \"unclosed")).is_err());
        assert!(exec_argv(&entry("app --file=%f")).is_err());
        assert!(exec_argv(&entry("app %Z")).is_err());
    }
    #[test]
    fn search_ranks_prefix_before_keyword_and_subsequence() {
        let mut a = entry("app");
        a.name = "Terminal".into();
        let mut b = entry("app");
        b.name = "Console".into();
        b.keywords.push("terminal".into());
        let all = vec![b, a];
        let found = search(&all, "term");
        assert_eq!(found[0].name, "Terminal");
        assert_eq!(found[1].name, "Console");
        assert_eq!(search(&all, "trml")[0].name, "Terminal");
    }
    #[test]
    fn user_hidden_entry_masks_system_copy() {
        let root = std::env::temp_dir().join(format!("aura-xdg-test-{}", std::process::id()));
        let user = root.join("user");
        let system = root.join("system");
        std::fs::create_dir_all(user.join("applications")).unwrap();
        std::fs::create_dir_all(system.join("applications/tools")).unwrap();
        std::fs::write(
            user.join("applications/editor.desktop"),
            "[Desktop Entry]\nType=Application\nName=Masked\nHidden=true\nExec=editor",
        )
        .unwrap();
        std::fs::write(
            system.join("applications/editor.desktop"),
            "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor",
        )
        .unwrap();
        std::fs::write(
            system.join("applications/tools/viewer.desktop"),
            "[Desktop Entry]\nType=Application\nName=Viewer\nExec=viewer",
        )
        .unwrap();
        let entries = discover_in(&[user, system]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].app_id, "tools-viewer.desktop");
        assert_eq!(entries[0].name, "Viewer");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn hidden_entries_and_non_application_entries_are_filtered() {
        assert!(
            parse_entry(
                "[Desktop Entry]\nType=Application\nName=Hidden\nExec=true\nHidden=true",
                "x".into(),
                PathBuf::new()
            )
            .is_none()
        );
        assert!(
            parse_entry(
                "[Desktop Entry]\nType=Link\nName=URL\nExec=true",
                "x".into(),
                PathBuf::new()
            )
            .is_none()
        );
    }
}
