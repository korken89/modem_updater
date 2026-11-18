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
//! use modem_updater::{ModemUpdater, pin_reset};
//!
//! let lister = Lister::new();
//! let selector: DebugProbeSelector = "2e8a:000c".parse().unwrap();
//! let mut probe = lister.open(selector).unwrap();
//! pin_reset(&mut probe).unwrap();
//! let mut session = probe
//!     .attach("nRF9151_xxAA", Permissions::new())
//!     .unwrap();
//! let mut updater = ModemUpdater::new(&mut session);
//! updater.program_and_verify("modem_update.zip").unwrap();
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

pub mod protocol;

pub use protocol::{
    Clock, DEFAULT_RESPONSE_TIMEOUT_MS, HfxoConfig, HfxoSource, HfxoStatus, IPC_CHUNK_ALIGNMENT,
    IPC_MAX_BUFFER_SIZE, IPC_PIPELINED_MAX_BUFFER_SIZE, ModemMemory, PREPARE_RESPONSE_TIMEOUT_MS,
    ProgrammedHfxo, ProtocolEngine, ProtocolError, TargetProfile, hfxo_addresses, hfxo_defaults,
};

#[cfg(feature = "std")]
mod host;

#[cfg(feature = "std")]
pub use host::{ModemUpdateError, ModemUpdater, ProbeRsMemory, StdClock, pin_reset};
