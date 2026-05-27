// Copyright 2025 Jared Wolff
//
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Apache-2.0 applies with the trademark modification described in
// LICENSE-APACHE.

//! Modem firmware update utility for nRF91 Series.
//!
//! The crate is split into two layers:
//!
//! * [`protocol`] — `no_std`-compatible nRF91 IPC DFU state machine. Operates
//!   on the [`protocol::ModemMemory`] and [`protocol::Clock`] traits, has no
//!   probe-rs / filesystem dependencies, and can be reused on-device to drive
//!   modem firmware updates from external flash.
//! * [`host`] (enabled by the default `std` feature) — PC-side glue that
//!   adapts probe-rs to [`protocol::ModemMemory`], parses Nordic's `.zip`
//!   firmware packages, and exposes the convenience [`ModemUpdater`] API.
//!
//! # Example
//! ```no_run
//! use probe_rs::{
//!     probe::{list::Lister, DebugProbeSelector},
//!     Permissions,
//! };
//! use modem_updater::ModemUpdater;
//!
//! let lister = Lister::new();
//! let probe = lister.open(DebugProbeSelector {
//!     vendor_id: 0x2e8a,
//!     product_id: 0x000c,
//!     interface: None,
//!     serial_number: None,
//! }).unwrap();
//! let mut session = probe
//!     .attach("nRF9151_xxAA", Permissions::new().allow_erase_all())
//!     .unwrap();
//! let mut updater = ModemUpdater::new(&mut session);
//! updater.program_and_verify("modem_update.zip").unwrap();
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

pub mod protocol;

pub use protocol::{
    hfxo_addresses, hfxo_defaults, Clock, HfxoStatus, ModemMemory, ProtocolEngine, ProtocolError,
    TargetProfile, DEFAULT_RESPONSE_TIMEOUT_MS, IPC_MAX_BUFFER_SIZE, IPC_PIPELINED_MAX_BUFFER_SIZE,
};

#[cfg(feature = "std")]
mod host;

#[cfg(feature = "std")]
pub use host::{ModemUpdateError, ModemUpdater, ProbeRsMemory, StdClock};
