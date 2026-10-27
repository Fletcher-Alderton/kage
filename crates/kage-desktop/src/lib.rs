//! The kage client: one window over one engine link.
//!
//! The shell is being rebuilt on egui (specs/epics/, E01 onward); this
//! commit carries the framework-free spine: preferences, the launch
//! gate, clocks, timings, the paced text reveal and the transports
//! that front `kage-client`. The views return with their epics.

// The icon catalog and the brand art callers return with their views;
// until then the embeds are carried but unreferenced, and CI denies
// dead code.
#[cfg(target_os = "linux")]
mod appearance;
#[allow(dead_code)]
mod assets;
pub mod clock;
mod external;
pub mod fonts;
/// Frame-time instrumentation behind `KAGE_FRAME_TIMES`, for the
/// NFR-01 budget.
pub mod frame_times;
pub mod gate;
// The window icon decodes a PNG through `image`, which the wasm
// bundle does not carry: the browser canvas takes no window icon.
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub mod icon;
#[allow(dead_code)]
pub mod icons;
pub mod launch;
pub mod logos;
mod modes;
#[cfg(not(target_arch = "wasm32"))]
mod picker;
pub mod prefs;
// The views that call the paced reveal return with E04; until then its
// API is carried but unreferenced, and CI denies dead code.
#[allow(dead_code)]
pub mod reveal;
mod shapes;
#[allow(dead_code)]
pub mod store;
pub mod theme;
pub mod themes;
pub mod timing;
pub mod transport;
pub mod ui;
pub mod update;
/// The browser entry: boots the shell once the page's connection form
/// is submitted. Native targets ignore it.
#[cfg(target_arch = "wasm32")]
pub mod web;
// The vim layer's key router arrives with E03; until then the state
// machine is carried but unreferenced, and CI denies dead code.
#[allow(dead_code)]
pub mod vim;

/// Reports a problem where the platform can show it: stderr on the
/// desktop, the browser console on the web.
pub(crate) fn warn(message: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::warn_1(&format!("kage-desktop: {message}").into());
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("kage-desktop: {message}");
}
