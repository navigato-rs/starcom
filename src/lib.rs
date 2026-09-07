//! Building blocks for the Starcom tmux GUI.
//!
//! Protocol and terminal modules own neither a network connection nor a window.
//! Optional SSH and desktop modules add explicitly authorized live sessions.

pub mod command;
pub mod control;
pub mod core;
pub mod input;
#[cfg(feature = "ssh")]
pub mod inspect;
pub mod reconnect;
pub mod replay;
#[cfg(feature = "ssh")]
pub mod session;
#[cfg(feature = "ssh")]
pub mod sessions;
#[cfg(feature = "ssh")]
pub mod sftp;
pub mod snapshot;
#[cfg(feature = "ssh")]
pub use sunset_client::{self as ssh, config as ssh_config};
#[cfg(feature = "gui")]
pub mod store;
pub mod terminal;

#[cfg(feature = "gui")]
pub mod desktop;
#[cfg(feature = "gui")]
mod dialog;
#[cfg(feature = "gui")]
mod ui;
#[cfg(all(feature = "gui", target_os = "linux"))]
mod wayland_drop;
#[cfg(feature = "gui")]
mod window;
#[cfg(feature = "gui")]
mod window_runtime;
#[cfg(feature = "gui")]
mod workspace;

#[cfg(feature = "gui")]
pub const SUPPORT: navigato_support::Info = navigato_support::Info {
    app: navigato_support::App::Starcom,
    version: env!("CARGO_PKG_VERSION"),
    revision: option_env!("GITHUB_SHA"),
    private_email: option_env!("NAVIGATO_PRIVATE_REPORT_EMAIL"),
};
