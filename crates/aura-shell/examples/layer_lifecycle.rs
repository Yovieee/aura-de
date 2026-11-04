//! Real NULL-buffer unmap/remap regression against an isolated Aura compositor.
//!
//! Run with WAYLAND_DISPLAY and AURA_CONTROL_SOCKET pointing to the same empty
//! Aura instance. External runners should impose a timeout (e.g. 15 seconds).
//! No shell or other exclusive layer clients may run concurrently.
use aura_shell::ipc::control_command;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
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
    protocol::{wl_output, wl_shm, wl_surface},
};

struct Fixture {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    width: u32,
    height: u32,
    frames: u32,
    configures: u32,
    drawing: bool,
}

fn configure_role(layer: &LayerSurface) {
    // NULL-buffer unmap resets the layer-shell role state. These must be sent
    // again before the new empty commit requests a configure for remapping.
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, 32);
    layer.set_exclusive_zone(32);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
}

fn check_reservations(top: i64, mapped: u64) -> Result<(), Box<dyn std::error::Error>> {
    let response = control_command("status").map_err(std::io::Error::other)?;
    let status: serde_json::Value = serde_json::from_str(&response)?;
    assert_eq!(
        status["reserved_top"].as_f64(),
        Some(top as f64),
        "{status}"
    );
    assert_eq!(status["reserved_bottom"].as_f64(), Some(0.0), "{status}");
    assert_eq!(status["mapped_layers"].as_u64(), Some(mapped), "{status}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Require explicit control selection: this fixture changes panel reservations
    // and must never silently run against a user's interactive desktop.
    if std::env::var_os("AURA_CONTROL_SOCKET").is_none() {
        return Err("AURA_CONTROL_SOCKET must select an isolated Aura compositor".into());
    }
    check_reservations(0, 0)?;
    let connection = Connection::connect_to_env()?;
    let (globals, mut events) = registry_queue_init(&connection)?;
    let qh = events.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let layer = shell.create_layer_surface(
        &qh,
        compositor.create_surface(&qh),
        Layer::Top,
        Some("aura-lifecycle-test"),
        None,
    );
    configure_role(&layer);
    layer.commit();
    let mut fixture = Fixture {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        pool: SlotPool::new(1920 * 32 * 4, &shm)?,
        shm,
        layer,
        width: 1,
        height: 32,
        frames: 0,
        configures: 0,
        drawing: true,
    };
    while fixture.frames < 2 {
        events.blocking_dispatch(&mut fixture)?;
    }
    events.roundtrip(&mut fixture)?;
    check_reservations(32, 1)?;

    fixture.drawing = false;
    fixture.layer.wl_surface().attach(None, 0, 0);
    fixture.layer.commit();
    events.roundtrip(&mut fixture)?;
    check_reservations(0, 0)?;

    // The same wl_surface and layer role remain alive through NULL-buffer unmap.
    let unmap_configures = fixture.configures;
    fixture.frames = 0;
    fixture.drawing = true;
    configure_role(&fixture.layer);
    fixture.layer.commit();
    while fixture.frames < 2 {
        events.blocking_dispatch(&mut fixture)?;
    }
    events.roundtrip(&mut fixture)?;
    assert!(
        fixture.configures > unmap_configures,
        "remapping must negotiate a new configure"
    );
    check_reservations(32, 1)?;

    fixture.drawing = false;
    fixture.layer.wl_surface().attach(None, 0, 0);
    fixture.layer.commit();
    events.roundtrip(&mut fixture)?;
    check_reservations(0, 0)?;
    println!(
        "Layer lifecycle passed: map -> NULL-buffer unmap -> reconfigure/remap -> unmap; role stayed alive and work area recovered"
    );
    Ok(())
}

impl Fixture {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if !self.drawing || self.frames >= 2 {
            return;
        }
        assert!(self.width > 0 && self.width <= 8192 && self.height <= 8192);
        let (buffer, pixels) = self
            .pool
            .create_buffer(
                self.width as i32,
                self.height as i32,
                self.width as i32 * 4,
                wl_shm::Format::Argb8888,
            )
            .expect("SHM allocation");
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&0xff285a88u32.to_ne_bytes());
        }
        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        buffer.attach_to(surface).expect("SHM attachment");
        self.layer.commit();
    }
}

impl CompositorHandler for Fixture {
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
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frames += 1;
        self.draw(qh);
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
impl LayerShellHandler for Fixture {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        panic!("compositor closed the fixture layer unexpectedly");
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configures += 1;
        self.width = configure.new_size.0.max(1);
        self.height = configure.new_size.1.max(1);
        self.draw(qh);
    }
}
impl OutputHandler for Fixture {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl ShmHandler for Fixture {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
delegate_registry!(Fixture);
impl ProvidesRegistryState for Fixture {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
smithay_client_toolkit::delegate_dispatch2!(Fixture);
