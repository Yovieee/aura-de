//! Keyboard-interactive XDG application launcher using an opaque SHM fallback.
use aura_shell::{
    client::{self, Canvas, DesktopUi, PanelSpec},
    desktop::{self, DesktopEntryItem},
};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A modal prompt that can only approve after an explicit Allow action.
struct ScreenCastConsent {
    app_label: String,
    allow_selected: bool,
    finished: bool,
    approved: Arc<AtomicBool>,
}
impl ScreenCastConsent {
    fn new(app_id: &str, approved: Arc<AtomicBool>) -> Self {
        let app_label: String = app_id
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();
        Self {
            app_label: if app_label.trim().is_empty() {
                "Unknown application".into()
            } else {
                app_label
            },
            allow_selected: false,
            finished: false,
            approved,
        }
    }
    fn decide(&mut self, allow: bool) {
        if self.finished {
            return;
        }
        self.approved.store(allow, Ordering::SeqCst);
        self.finished = true;
    }
}
impl DesktopUi for ScreenCastConsent {
    fn draw(&mut self, _: usize, c: &mut Canvas<'_>) {
        // Consent must stay readable even when compositor-owned glass is enabled.
        c.clear(0xff192534);
        c.text(24, 26, "Share your screen?", 0xfff0f5fc, 2);
        c.text(24, 72, "Application:", 0xffb8c8dc, 1);
        let chars: Vec<_> = self.app_label.chars().collect();
        for (line, chunk) in chars.chunks(66).take(2).enumerate() {
            c.text(
                24,
                92 + line as i32 * 16,
                &chunk.iter().collect::<String>(),
                0xfff0f5fc,
                1,
            );
        }
        c.text(
            24,
            138,
            "Allow access to the entire Aura display?",
            0xffedf3fc,
            1,
        );
        c.text(
            24,
            158,
            "Screen contents remain shared until sharing is stopped.",
            0xffb8c8dc,
            1,
        );
        c.text(
            24,
            192,
            "Tab / Left / Right to select, Enter to choose, Esc to deny.",
            0xffb8c8dc,
            1,
        );
        for (x, label, selected) in [
            (280, "Deny", !self.allow_selected),
            (424, "Allow", self.allow_selected),
        ] {
            c.rect(
                x,
                222,
                128,
                40,
                if selected { 0xff5489bd } else { 0xff2a3c51 },
            );
            c.text(x + 24, 238, label, 0xfff0f5fc, 1);
        }
    }
    fn key(&mut self, key: u32, _: Option<&str>) -> bool {
        if self.finished {
            return true;
        }
        match key {
            0xff1b => self.decide(false),
            0xff09 | 0xfe20 => self.allow_selected = !self.allow_selected,
            0xff51 => self.allow_selected = false,
            0xff53 => self.allow_selected = true,
            0xff0d | 0xff8d => self.decide(self.allow_selected),
            _ => {}
        }
        self.finished
    }
    fn pointer(&mut self, _: usize, x: f64, y: f64, pressed: bool) {
        if pressed && (222.0..262.0).contains(&y) {
            if (280.0..408.0).contains(&x) {
                self.decide(false);
            } else if (424.0..552.0).contains(&x) {
                self.decide(true);
            }
        }
    }
    fn finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consent() -> (ScreenCastConsent, Arc<AtomicBool>) {
        let decision = Arc::new(AtomicBool::new(false));
        (
            ScreenCastConsent::new("org.example.Capture", decision.clone()),
            decision,
        )
    }
    #[test]
    fn consent_defaults_to_denial_and_requires_explicit_allow_selection() {
        let (mut ui, decision) = consent();
        assert!(ui.key(0xff0d, None));
        assert!(!decision.load(Ordering::SeqCst));
        // Input already queued after a completed decision cannot change it.
        ui.key(0xff09, None);
        ui.key(0xff0d, None);
        assert!(!decision.load(Ordering::SeqCst));
        let (mut ui, decision) = consent();
        assert!(!ui.key(0xff09, None));
        assert!(ui.key(0xff0d, None));
        assert!(decision.load(Ordering::SeqCst));
    }
    #[test]
    fn consent_escape_and_pointer_misses_never_approve() {
        let (mut ui, decision) = consent();
        ui.pointer(0, 450.0, 240.0, false);
        ui.pointer(0, 10.0, 240.0, true);
        assert!(!ui.finished());
        ui.key(0xff09, None);
        assert!(ui.key(0xff1b, None));
        assert!(!decision.load(Ordering::SeqCst));
    }
    #[test]
    fn consent_pointer_requires_a_press_inside_allow_button() {
        let (mut ui, decision) = consent();
        ui.pointer(0, 450.0, 240.0, true);
        assert!(ui.finished());
        assert!(decision.load(Ordering::SeqCst));
        let (mut ui, decision) = consent();
        ui.pointer(0, 300.0, 240.0, true);
        assert!(ui.finished());
        assert!(!decision.load(Ordering::SeqCst));
    }
    #[test]
    fn consent_rejects_smoke_mode_and_bounds_untrusted_app_text() {
        for args in [
            vec![
                "--screen-cast-consent",
                "org.example.Capture",
                "--smoke-test",
            ],
            vec![
                "--smoke-test",
                "--screen-cast-consent",
                "org.example.Capture",
            ],
            vec!["--screen-cast-consent", "--smoke-test"],
        ] {
            assert!(parse_mode(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
        let ui = ScreenCastConsent::new(
            &format!("\n{}\x1b", "x".repeat(512)),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(ui.app_label.chars().count(), 120);
        assert!(!ui.app_label.chars().any(char::is_control));
    }
}

#[derive(Debug, PartialEq)]
enum Mode {
    Launcher,
    Search(String),
    ScreenCastConsent(String),
}
fn parse_mode(args: &[String]) -> Result<Mode, &'static str> {
    match args {
        [] => Ok(Mode::Launcher),
        [flag] if flag == "--smoke-test" => Ok(Mode::Launcher),
        [flag] if flag == "--search" => Ok(Mode::Search(String::new())),
        [flag, query] if flag == "--search" => Ok(Mode::Search(query.clone())),
        [flag, app_id] if flag == "--screen-cast-consent" && !app_id.starts_with("--") => {
            Ok(Mode::ScreenCastConsent(app_id.clone()))
        }
        _ => Err(
            "Usage: aura-launcher [--search QUERY | --smoke-test | --screen-cast-consent APP_ID]",
        ),
    }
}

struct Launcher {
    entries: Vec<DesktopEntryItem>,
    query: String,
    selected: usize,
    error: Option<String>,
    reduced_transparency: bool,
}
impl DesktopUi for Launcher {
    fn draw(&mut self, _: usize, c: &mut Canvas<'_>) {
        c.panel_background(0xff192534, self.reduced_transparency);
        c.rect(12, 12, c.width.saturating_sub(24), 38, 0xff2a3c51);
        c.text(24, 26, &format!("> {}_", self.query), 0xfff0f5fc, 1);
        let matches = desktop::search(&self.entries, &self.query);
        if matches.is_empty() {
            c.text(24, 76, "No applications found", 0xffb1c2d9, 1);
        }
        for (i, entry) in matches.iter().take(8).enumerate() {
            let y = 64 + i as i32 * 30;
            if i == self.selected {
                c.rect(12, y - 5, c.width.saturating_sub(24), 28, 0xff365d84);
            }
            c.text(24, y + 4, &entry.name, 0xffedf3fc, 1);
        }
        c.text(
            20,
            c.height as i32 - 34,
            "Type to search | Up/Down | Enter | Esc",
            0xffa4bbd3,
            1,
        );
        if let Some(error) = &self.error {
            c.text(20, c.height as i32 - 18, error, 0xffff9e9e, 1);
        }
    }
    fn key(&mut self, key: u32, text: Option<&str>) -> bool {
        self.error = None;
        match key {
            0xff1b => return true,
            0xff08 => {
                self.query.pop();
                self.selected = 0;
            }
            0xff52 => {
                self.selected = self.selected.saturating_sub(1);
            }
            0xff54 => {
                self.selected = (self.selected + 1).min(
                    desktop::search(&self.entries, &self.query)
                        .len()
                        .min(8)
                        .saturating_sub(1),
                );
            }
            0xff0d | 0xff8d => {
                if let Some(entry) = desktop::search(&self.entries, &self.query).get(self.selected)
                {
                    match desktop::launch(entry) {
                        Ok(()) => return true,
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            _ => {
                if let Some(text) = text
                    && !text.chars().any(char::is_control)
                    && self.query.len() + text.len() <= 256
                {
                    self.query.push_str(text);
                    self.selected = 0;
                }
            }
        }
        false
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = match parse_mode(&args) {
        Ok(mode) => mode,
        Err(error) => {
            if args.iter().any(|arg| arg == "--screen-cast-consent") {
                println!("denied");
            }
            return Err(error.into());
        }
    };
    if let Mode::ScreenCastConsent(app_id) = mode {
        let approved = Arc::new(AtomicBool::new(false));
        let result = client::run(
            vec![PanelSpec {
                namespace: "aura-screen-cast-consent",
                width: 600,
                height: 286,
                anchor: Anchor::empty(),
                layer: Layer::Overlay,
                exclusive_zone: 0,
                keyboard: KeyboardInteractivity::Exclusive,
            }],
            Box::new(ScreenCastConsent::new(&app_id, approved.clone())),
        );
        println!(
            "{}",
            if result.is_ok() && approved.load(Ordering::SeqCst) {
                "approved"
            } else {
                "denied"
            }
        );
        return result;
    }
    let entries = desktop::discover();
    if let Mode::Search(query) = mode {
        for entry in desktop::search(&entries, &query) {
            println!("{}\t{}", entry.app_id, entry.name);
        }
        return Ok(());
    }
    let panel = PanelSpec {
        namespace: "aura-launcher",
        width: 600,
        height: 360,
        anchor: Anchor::empty(),
        layer: Layer::Overlay,
        exclusive_zone: 0,
        keyboard: KeyboardInteractivity::Exclusive,
    };
    client::run(
        vec![panel],
        Box::new(Launcher {
            entries,
            query: String::new(),
            selected: 0,
            error: None,
            reduced_transparency: std::env::var("AURA_GLASS_QUALITY")
                .is_ok_and(|value| value == "reduced-transparency"),
        }),
    )
}
