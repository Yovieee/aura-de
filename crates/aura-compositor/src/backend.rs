//! Presentation and device events shared by nested and native sessions.
use crate::{Backend, Config};
use smithay::{
    backend::{
        renderer::gles::{GlesRenderer, GlesTarget},
        winit::{self, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    },
    utils::{Physical, Rectangle, Size, Transform},
};
use std::error::Error;

pub(crate) struct OutputInfo {
    pub name: String,
    pub physical_size: (i32, i32),
    pub make: String,
    pub model: String,
    pub refresh: i32,
}

pub(crate) enum BackendEvent {
    Winit(WinitEvent),
    #[cfg(feature = "native")]
    Native(crate::native::NativeEvent),
}

pub(crate) enum Presenter {
    Winit(Box<(WinitGraphicsBackend<GlesRenderer>, WinitEventLoop)>),
    #[cfg(feature = "native")]
    Drm(Box<crate::native::NativeBackend>),
}

impl Presenter {
    /// Acquire the display/seat before publishing any Wayland or control socket.
    pub fn new(config: &Config) -> Result<Self, Box<dyn Error>> {
        match config.backend {
            Backend::Winit => {
                let attributes = ::winit::window::Window::default_attributes()
                    .with_title("Aura DE — Nested")
                    .with_inner_size(::winit::dpi::PhysicalSize::new(
                        config.size.0 as u32,
                        config.size.1 as u32,
                    ));
                Ok(Self::Winit(Box::new(winit::init_from_attributes::<
                    GlesRenderer,
                >(attributes)?)))
            }
            Backend::Drm => {
                #[cfg(feature = "native")]
                {
                    Ok(Self::Drm(Box::new(crate::native::NativeBackend::new()?)))
                }
                #[cfg(not(feature = "native"))]
                {
                    Err("the drm backend requires a build with --features native".into())
                }
            }
        }
    }
    pub fn size(&self) -> Size<i32, Physical> {
        match self {
            Self::Winit(backend) => backend.0.window_size(),
            #[cfg(feature = "native")]
            Self::Drm(backend) => backend.size(),
        }
    }
    pub fn output_info(&self) -> OutputInfo {
        match self {
            Self::Winit(_) => OutputInfo {
                name: "AURA-1".into(),
                physical_size: (300, 200),
                make: "Aura".into(),
                model: "Nested".into(),
                refresh: 60_000,
            },
            #[cfg(feature = "native")]
            Self::Drm(backend) => {
                let info = backend.output_info();
                OutputInfo {
                    name: info.name.clone(),
                    physical_size: info.physical_size,
                    make: info.make.clone(),
                    model: info.model.clone(),
                    refresh: info.refresh,
                }
            }
        }
    }
    pub fn poll_events(&mut self) -> Result<Vec<BackendEvent>, Box<dyn Error>> {
        match self {
            Self::Winit(backend) => {
                let mut events = Vec::new();
                let status = backend
                    .1
                    .dispatch_new_events(|event| events.push(BackendEvent::Winit(event)));
                if matches!(status, ::winit::platform::pump_events::PumpStatus::Exit(_)) {
                    events.push(BackendEvent::Winit(WinitEvent::CloseRequested));
                }
                Ok(events)
            }
            #[cfg(feature = "native")]
            Self::Drm(backend) => Ok(backend
                .poll_events()?
                .into_iter()
                .map(BackendEvent::Native)
                .collect()),
        }
    }
    pub fn can_render(&self) -> bool {
        match self {
            Self::Winit(_) => true,
            #[cfg(feature = "native")]
            Self::Drm(backend) => backend.can_render(),
        }
    }
    pub fn output_transform(&self) -> Transform {
        match self {
            Self::Winit(_) => Transform::Flipped180,
            #[cfg(feature = "native")]
            Self::Drm(_) => Transform::Normal,
        }
    }
    /// Native readback orientation and cursor exclusion require hardware validation.
    pub fn supports_capture(&self) -> bool {
        matches!(self, Self::Winit(_))
    }
    pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>), Box<dyn Error>> {
        match self {
            Self::Winit(backend) => Ok(backend.0.bind()?),
            #[cfg(feature = "native")]
            Self::Drm(backend) => backend.bind(),
        }
    }
    pub fn submit(
        &mut self,
        damage: Option<&[Rectangle<i32, Physical>]>,
    ) -> Result<(), Box<dyn Error>> {
        match self {
            Self::Winit(backend) => Ok(backend.0.submit(damage)?),
            #[cfg(feature = "native")]
            Self::Drm(backend) => backend.submit(damage),
        }
    }
    pub fn change_vt(&mut self, vt: i32) -> Result<(), Box<dyn Error>> {
        match self {
            #[cfg(feature = "native")]
            Self::Drm(backend) => backend.change_vt(vt),
            Self::Winit(_) => {
                let _ = vt;
                Err("VT switching requires the drm backend".into())
            }
        }
    }
}
