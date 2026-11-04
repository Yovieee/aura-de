//! Shared software-rendered layer-shell client for the shell and launcher.
//! Opaque SHM is the default; optional compositor-owned glass uses transparent panel backgrounds.
pub mod client;
pub mod desktop;
pub mod ipc;
