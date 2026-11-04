//! Keyboard-interactive XDG application launcher using an opaque SHM fallback.
use aura_shell::{
    client::{self, Canvas, DesktopUi, PanelSpec},
    desktop::{self, DesktopEntryItem},
};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let entries = desktop::discover();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--search") {
        for entry in desktop::search(&entries, args.get(1).map(String::as_str).unwrap_or("")) {
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
