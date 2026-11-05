//! Ordered scene elements: client surfaces and compositor-owned window chrome.
use smithay::backend::renderer::{
    element::{
        render_elements, solid::SolidColorRenderElement, surface::WaylandSurfaceRenderElement,
    },
    gles::GlesRenderer,
};
render_elements! {
    pub SceneElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
}
