//! Client + decision logic for a [Seerr](https://docs.seerr.dev/)
//! (Jellyseerr/Overseerr) server integration ("Discover").
//!
//! Layout:
//! - [`client`]/[`models`]/[`error`]/[`url`]/[`util`]: the hand-written
//!   reqwest/serde/tokio HTTP client and its wire DTOs.
//! - [`types`]: plain, app-facing records/enums (`SeerrCard`,
//!   `SeerrMovieDetail`, `SeerrAvailability`, ...) for the app crate to
//!   consume directly.
//! - [`logic`]: the pure, network-free decision functions worth unit
//!   testing on their own (availability mapping, season requestability,
//!   POST-vs-PUT, image URL building).
//! - [`store`]: the on-disk per-account config store (`<data_dir>/seerr.json`).
//!   Every entry point takes `data_dir` and the active `(server_url,
//!   user_id)` pair as plain arguments rather than reading them from any
//!   ambient session state; the app supplies its own active-session
//!   identity explicitly on every call.
//! - [`session`]: [`session::SeerrSession`], the live per-account connection
//!   (auth handle + 60s home cache + process-lifetime genre cache) and the
//!   free functions around it (`connect`/`open`/`status`/`disconnect`).
//!   The app runs on gpui's own async executor, so `SeerrSession`'s
//!   methods are exposed as plain `async fn`s directly -- there is no
//!   blocking wrapper, and the app decides how (or whether) to spawn them.

mod client;
mod error;
mod logic;
mod models;
mod session;
mod store;
mod types;
mod util;

pub mod url;

pub use client::{BrowseFilters, SeerrAuthMethod, SeerrClient};
pub use error::SeerrError;
pub use session::{
    connect, disconnect, open, status, ConnectArgs, SeerrSession, SeerrSessionError,
};
pub use store::{SeerrConfigEntry, SeerrConfigStore};
pub use types::*;
