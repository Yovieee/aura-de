//! `aura-core`: Pure business logic for the Aura Desktop Environment.
//!
//! This crate intentionally has zero dependencies on Wayland sockets, GPU APIs,
//! or OS threads so that all layout, animation, Dock magnification, and optical
//! material invariants can be deterministically unit-tested.

pub mod dock;
pub mod layout;
pub mod material;
pub mod spring;

pub use dock::{DockConfig, DockItemLayout, DockMagnifier};
pub use layout::{
    LayoutMode, Rect, SnapZone, TrafficLightAction, WindowId, WindowNode, WindowStateFlags,
    WorkspaceManager,
};
pub use material::{GlassQualityPreset, LiquidGlassError, LiquidGlassMaterial};
pub use spring::{Spring1D, SpringConfig, SpringRect};
