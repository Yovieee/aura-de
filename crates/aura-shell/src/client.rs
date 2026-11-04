use font8x8::UnicodeFonts;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

pub struct PanelSpec {
    pub namespace: &'static str,
    pub width: u32,
    pub height: u32,
    pub anchor: Anchor,
    pub layer: Layer,
    pub exclusive_zone: i32,
    pub keyboard: KeyboardInteractivity,
}

/// Minimal event-driven UI contract. Return true from a key handler to exit.
pub trait DesktopUi {
    fn draw(&mut self, panel: usize, canvas: &mut Canvas<'_>);
    fn key(&mut self, _keysym: u32, _text: Option<&str>) -> bool {
        false
    }
    fn pointer(&mut self, _panel: usize, _x: f64, _y: f64, _pressed: bool) {}
}

pub struct Canvas<'a> {
    pub pixels: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    /// True only when the session explicitly enables Aura compositor-owned glass.
    pub compositor_glass: bool,
}
impl Canvas<'_> {
    /// Leave panel fill to the compositor only under the explicit opt-in contract.
    /// Client glyphs/icons remain normal premultiplied ARGB SHM content.
    pub fn panel_background(&mut self, opaque_color: u32, reduced_transparency: bool) {
        self.clear(if self.compositor_glass && !reduced_transparency {
            0
        } else {
            opaque_color | 0xff000000
        });
    }
    pub fn clear(&mut self, color: u32) {
        for p in self.pixels.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&color.to_ne_bytes());
        }
    }
    pub fn rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: u32) {
        for row in y.max(0)..(y.saturating_add(height as i32)).min(self.height as i32) {
            for col in x.max(0)..(x.saturating_add(width as i32)).min(self.width as i32) {
                let i = (row as usize * self.width as usize + col as usize) * 4;
                self.pixels[i..i + 4].copy_from_slice(&color.to_ne_bytes());
            }
        }
    }
    pub fn text(&mut self, x: i32, y: i32, text: &str, color: u32, scale: u32) {
        let mut cursor = x;
        for c in text.chars() {
            if cursor + (8 * scale) as i32 > self.width as i32 {
                break;
            }
            if let Some(glyph) = font8x8::BASIC_FONTS
                .get(c)
                .or_else(|| font8x8::BASIC_FONTS.get('?'))
            {
                for (row, bits) in glyph.into_iter().enumerate() {
                    for col in 0..8 {
                        if bits & (1 << col) != 0 {
                            self.rect(
                                cursor + col * scale as i32,
                                y + row as i32 * scale as i32,
                                scale,
                                scale,
                                color,
                            );
                        }
                    }
                }
            }
            cursor += (8 * scale) as i32;
        }
    }
}
struct Panel {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
}
struct Client {
    registry: RegistryState,
    seat: SeatState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    panels: Vec<Panel>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    ui: Box<dyn DesktopUi>,
    exit: bool,
    smoke: bool,
    frames: Vec<u32>,
    compositor_glass: bool,
}
pub fn run(
    specs: Vec<PanelSpec>,
    ui: Box<dyn DesktopUi>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut events) = registry_queue_init(&conn)?;
    let qh = events.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new(4 * 1024 * 1024, &shm)?;
    let panel_count = specs.len();
    let panels = specs
        .into_iter()
        .map(|spec| {
            let surface = compositor.create_surface(&qh);
            let layer =
                shell.create_layer_surface(&qh, surface, spec.layer, Some(spec.namespace), None);
            layer.set_anchor(spec.anchor);
            layer.set_size(spec.width, spec.height);
            layer.set_keyboard_interactivity(spec.keyboard);
            layer.set_exclusive_zone(spec.exclusive_zone);
            layer.commit();
            Panel {
                layer,
                width: spec.width.max(1),
                height: spec.height.max(1),
                configured: false,
            }
        })
        .collect();
    let mut state = Client {
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        outputs: OutputState::new(&globals, &qh),
        shm,
        pool,
        panels,
        keyboard: None,
        pointer: None,
        ui,
        exit: false,
        smoke: std::env::args().any(|a| a == "--smoke-test"),
        frames: vec![0; panel_count],
        compositor_glass: std::env::var("AURA_COMPOSITOR_GLASS").is_ok_and(|value| value == "1"),
    };
    while !state.exit {
        events.blocking_dispatch(&mut state)?;
    }
    Ok(())
}
impl Client {
    fn draw(&mut self, index: usize, qh: &QueueHandle<Self>) {
        let panel = &self.panels[index];
        if !panel.configured {
            return;
        }
        let (w, h) = (panel.width, panel.height);
        // Bound allocations from compositor-provided sizes, including integer casts.
        if w > 8192 || h > 8192 {
            tracing::error!(w, h, "Unsupported layer size");
            self.exit = true;
            return;
        }
        let Ok((buffer, pixels)) =
            self.pool
                .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
        else {
            self.exit = true;
            return;
        };
        self.ui.draw(
            index,
            &mut Canvas {
                pixels,
                width: w,
                height: h,
                compositor_glass: self.compositor_glass,
            },
        );
        let surface = panel.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        if let Err(error) = buffer.attach_to(surface) {
            tracing::error!(%error,"Buffer attachment failed");
            self.exit = true;
            return;
        }
        panel.layer.commit();
    }
}
impl CompositorHandler for Client {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(i) = self
            .panels
            .iter()
            .position(|p| p.layer.wl_surface() == surface)
        {
            self.frames[i] += 1;
            if self.smoke && self.frames.iter().all(|n| *n >= 2) {
                tracing::info!("Layer-shell configure/SHM attach/frame smoke test passed");
                self.exit = true;
            } else {
                self.draw(i, qh);
            }
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Client {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl LayerShellHandler for Client {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(i) = self.panels.iter().position(|p| p.layer == *layer) {
            let p = &mut self.panels[i];
            if configure.new_size.0 > 0 {
                p.width = configure.new_size.0;
            }
            if configure.new_size.1 > 0 {
                p.height = configure.new_size.1;
            }
            p.configured = true;
            self.draw(i, qh);
        }
    }
}
impl SeatHandler for Client {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => tracing::warn!(%e,"Keyboard unavailable"),
            }
        }
        if cap == Capability::Pointer && self.pointer.is_none() {
            match self.seat.get_pointer(qh, &seat) {
                Ok(p) => self.pointer = Some(p),
                Err(e) => tracing::warn!(%e,"Pointer unavailable"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
        if cap == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}
impl KeyboardHandler for Client {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.exit |= self.ui.key(event.keysym.raw(), event.utf8.as_deref());
    }
    fn repeat_key(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        k: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.press_key(conn, qh, k, serial, event);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}
impl PointerHandler for Client {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if let Some(i) = self
                .panels
                .iter()
                .position(|p| p.layer.wl_surface() == &event.surface)
            {
                self.ui.pointer(
                    i,
                    event.position.0,
                    event.position.1,
                    matches!(event.kind, PointerEventKind::Press { button: 0x110, .. }),
                );
            }
        }
    }
}
impl ShmHandler for Client {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
delegate_registry!(Client);
impl ProvidesRegistryState for Client {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
smithay_client_toolkit::delegate_dispatch2!(Client);

#[cfg(test)]
mod tests {
    use super::Canvas;
    #[test]
    fn compositor_background_opt_in_preserves_content_and_opaque_fallback() {
        let mut pixels = [0u8; 16];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            width: 2,
            height: 2,
            compositor_glass: false,
        };
        canvas.panel_background(0xff182230, false);
        assert_eq!(&canvas.pixels[..4], &0xff182230u32.to_ne_bytes());
        canvas.compositor_glass = true;
        canvas.panel_background(0xff182230, false);
        assert!(canvas.pixels.iter().all(|p| *p == 0));
        canvas.rect(0, 0, 1, 1, 0xfff0f5fc);
        assert_eq!(&canvas.pixels[..4], &0xfff0f5fcu32.to_ne_bytes());
        assert!(canvas.pixels[4..].iter().all(|p| *p == 0));
        canvas.panel_background(0xff182230, true);
        assert_eq!(&canvas.pixels[..4], &0xff182230u32.to_ne_bytes());
    }
}
