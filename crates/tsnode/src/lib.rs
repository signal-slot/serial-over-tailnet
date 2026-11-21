//! `tsnode`: a small userspace Tailscale node for constrained targets.
//!
//! Layers, bottom up:
//! - [`crypto`], [`noise`], [`keys`]: primitives.
//! - [`controlbase`], [`hpack`]/[`h2`], [`controlconn`]: the ts2021 control-plane connection.
//! - [`wireguard`], [`disco`], [`derp`], [`stun`], [`magicsock`]: the data plane.
//! - [`netstack`]: smoltcp-based TCP termination on the node's tailnet addresses.
//! - [`node`]: the node's decisions (registration, map handling).
//!
//! These are state machines without I/O: they need only `alloc`, and the
//! time is passed in. The `std` feature (default) adds the drivers that run
//! them on threads with sockets: `node::Node`, `magicsock::MagicSock`,
//! `netstack::NetStack`, `derp::DerpClient`, `control` and `net`.
//!
//! Keys and nonces come from `getrandom`. On a target without an operating
//! system, register the hardware's random source with
//! `getrandom::register_custom_getrandom!`.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "std")]
pub mod control;
pub mod controlbase;
pub mod controlconn;
pub mod crypto;
pub mod derp;
pub mod disco;
pub mod filter;
pub mod h2;
pub mod hpack;
pub mod http1;
pub mod keys;
pub mod magicsock;
#[cfg(feature = "std")]
pub mod net;
pub mod netstack;
pub mod node;
pub mod noise;
pub mod stun;
pub mod time;
pub mod types;
pub mod wireguard;
