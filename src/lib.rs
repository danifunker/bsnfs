//! bsnfs: share folders on a modern host with old (and new) NFS clients.
//!
//! Layout:
//! - [`vfs`]: the shared-folder backend (handles, attributes, file ops),
//!   protocol-neutral so future services (FTP, HTTP) can reuse it.
//! - [`xdr`], [`rpc`], [`server`]: ONC RPC over UDP and TCP.
//! - [`portmap`], [`nfs`]: the RPC programs.
//! - [`admin`]: the management web UI.
//! - [`app`]: wires it all together from a [`config::Config`].

/// The version stamped into a distribution build (`BSNFS_VERSION`), or the
/// crate version for a plain `cargo build`.
pub const VERSION: &str = match option_env!("BSNFS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[macro_use]
pub mod log;

pub mod admin;
pub mod app;
pub mod config;
mod confine;
pub mod nfs;
pub mod portmap;
pub mod rpc;
pub mod server;
pub mod service;
pub mod vfs;
pub mod xdr;
