//! Shared component library (`docs/DESIGN-GUIDE.md` Part D) -- the
//! reusable, screen-agnostic building blocks the token-retrofit passes
//! (player_ui.rs's track picker/info overlay, settings.rs's sidebar
//! rebuild, root.rs's Server Switcher/Connect screen) are built from.

pub(crate) mod components;
pub(crate) mod motion;
pub(crate) mod popover;
pub(crate) mod spec_strip;
