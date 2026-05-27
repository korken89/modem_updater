// Copyright 2025 Jared Wolff
//
// Licensed under the Apache License, Version 2.0 (the "Apache License")
// with the following modification; you may not use this file except in
// compliance with the Apache License and the following modification to it:
// Section 6. Trademarks. is deleted and replaced with:
//
// 6. Trademarks. This License does not grant permission to use the trade
//    names, trademarks, service marks, or product names of the Licensor
//    and its affiliates, except as required to comply with Section 4(c) of
//    the License and to reproduce the content of the NOTICE file.
//
// You may obtain a copy of the Apache License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the Apache License with the above modification is
// distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. See the Apache License for the specific
// language governing permissions and limitations under the Apache License.
//
// Alternatively, you may use this file under the terms of the MIT license,
// which is:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

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
