// Copyright 2025 Jared Wolff
//
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Apache-2.0 applies with the trademark modification described in
// LICENSE-APACHE.

//! `no_std`-compatible nRF91 modem DFU protocol engine.
//!
//! This module implements the host side of the IPC DFU protocol used to
//! update modem firmware on nRF9160 and nRF9151 parts. It is independent of
//! probe-rs, the filesystem, and the firmware container format: callers
//! provide a [`ModemMemory`] for raw memory access (over SWD on a PC, over
//! the local bus on-device) and a [`Clock`] for timeouts.
//!
//! Typical sequence:
//! 1. Halt the application core and ensure UICR HFXO trim is programmed
//!    (see [`ProtocolEngine::hfxo_status`] and [`hfxo_defaults`]).
//! 2. Call [`ProtocolEngine::read_key_digest`] to pick the matching signed
//!    loader from the firmware package.
//! 3. Call [`ProtocolEngine::setup`] to configure IPC, SPU, mailbox, and
//!    reset the modem.
//! 4. Load the signed loader binary into modem-mapped RAM (caller-driven;
//!    on a PC this is typically a probe-rs flash algorithm).
//! 5. Call [`ProtocolEngine::start_loader`] to kick the IPC task and wait
//!    for the loader to acknowledge.
//! 6. Call [`ProtocolEngine::write_chunk`] in a loop, streaming firmware
//!    segments. Alternate `bank` between 0 and 1 in pipelined mode.
//! 7. Call [`ProtocolEngine::verify`] with the list of programmed ranges
//!    and compare the returned digest against the package's manifest.

#![allow(clippy::needless_doctest_main)]

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Maximum data payload per `WRITE` command in pipelined mode (one bank).
pub const IPC_PIPELINED_MAX_BUFFER_SIZE: usize = 0xE000;
/// Maximum data payload per `WRITE` command in non-pipelined mode.
pub const IPC_MAX_BUFFER_SIZE: usize = 0x10000;

/// Default `wait_and_ack` timeout in milliseconds. Sized for the heaviest
/// in-flight operations: 57 KB pipelined chunk writes and on-modem digest
/// computation across large ranges. Setup / loader-boot ACKs return in
/// well under a second; for that phase consider the much shorter
/// [`PREPARE_RESPONSE_TIMEOUT_MS`] so a wedged modem trips immediately.
pub const DEFAULT_RESPONSE_TIMEOUT_MS: u64 = 30_000;

/// Recommended `wait_and_ack` timeout for the prepare phase (setup,
/// loader boot, key-digest read). Real ACK times here are sub-second on
/// healthy boards; a tight bound lets the CLI catch a wedged modem
/// almost instantly and trigger its erase-and-reattach recovery.
pub const PREPARE_RESPONSE_TIMEOUT_MS: u64 = 2_000;

// ---------------------------------------------------------------------------
// Internal address map
// ---------------------------------------------------------------------------

// Mailbox layout in app-core RAM
const MAILBOX_HEADER_ADDR: u32 = 0x20000000;
const MAILBOX_HEADER_MAGIC: u32 = 0x80010000;
const MAILBOX_HEADER_PTR_ADDR: u32 = 0x20000004;
const MAILBOX_HEADER_PTR_VALUE: u32 = 0x2100000C;
const MAILBOX_HEADER_SIZE_ADDR: u32 = 0x20000008;
const MAILBOX_HEADER_SIZE_VALUE: u32 = 0x0003FC00;
const MAILBOX_COMMAND_ADDR: u32 = 0x2000000C;
const MAILBOX_ARG0_ADDR: u32 = 0x20000010;
const MAILBOX_ARG1_ADDR: u32 = 0x20000014;
const MAILBOX_ARG2_ADDR: u32 = 0x20000018;
const DATA_BUFFER_NON_PIPELINED: u32 = 0x20000018;
const DATA_BUFFER_PIPELINED_BASE: u32 = 0x2000001C;

// IPC peripheral
const IPC_TASKS_SEND0: u32 = 0x4002A004;
const IPC_EVENT_FAULT: u32 = 0x4002A100;
const IPC_EVENT_COMMAND: u32 = 0x4002A108;
const IPC_EVENT_DATA: u32 = 0x4002A110;

const IPC_ROUTE_ADDR: u32 = 0x500038A8;
const IPC_ROUTE_VALUE: u32 = 0x00000002;
const IPC_SEND_CNF0_ADDR: u32 = 0x4002A514;
const IPC_SEND_CNF0_VALUE: u32 = 0x00000002;
const IPC_SEND_CNF2_ADDR: u32 = 0x4002A51C;
const IPC_SEND_CNF2_VALUE: u32 = 0x00000008;
const IPC_GPMEM0_ADDR: u32 = 0x4002A610;
const IPC_GPMEM0_VALUE: u32 = 0x21000000;
const IPC_GPMEM1_ADDR: u32 = 0x4002A614;
const IPC_GPMEM1_VALUE: u32 = 0x00000000;
const IPC_RECEIVE_FAULT_ADDR: u32 = 0x4002A590;
const IPC_RECEIVE_FAULT_MASK: u32 = 0x00000001;
const IPC_RECEIVE_COMMAND_ADDR: u32 = 0x4002A598;
const IPC_RECEIVE_DATA_ADDR: u32 = 0x4002A5A0;

// Modem reset (RESET peripheral, secure alias)
const MODEM_RESET_FORCEOFF: u32 = 0x50005610;
const MODEM_RESET_NETWORK_FORCEOFF: u32 = 0x50005614;

// SPU RAM region permissions
const SPU_RAMREGION_BASE: u32 = 0x50003700;
const SPU_RAMREGION_PERM: u32 = 0x00000007;
const SPU_RAMREGION_COUNT: u32 = 32;

// UICR (HFXO trim)
const UICR_HFXOSR_ADDR: u32 = 0x00FF801C;
const UICR_HFXOCNT_ADDR: u32 = 0x00FF8020;
const UICR_HFXOSR_DEFAULT: u32 = 0x0000000E;
const UICR_HFXOCNT_DEFAULT: u32 = 0x00000020;
const UICR_ERASED: u32 = 0xFFFFFFFF;

// Loader commands
const CMD_WRITE: u32 = 0x3;
const CMD_VERIFY: u32 = 0x7;
const CMD_PIPELINE_WRITE: u32 = 0x9;

// Response codes (high byte)
const RESPONSE_PREFIX_MASK: u32 = 0xFF000000;
const RESPONSE_PAYLOAD_MASK: u32 = 0x00FFFFFF;
const ACK_PREFIX: u32 = 0xA5000000;
const NACK_PREFIX: u32 = 0x5A000000;

// ---------------------------------------------------------------------------
// Target profile
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TargetProfile {
    Nrf9160,
    #[default]
    Nrf9151,
}

impl TargetProfile {
    pub fn probe_rs_target_name(self) -> &'static str {
        match self {
            Self::Nrf9160 => "nRF9160_xxAA",
            Self::Nrf9151 => "nRF9151_xxAA",
        }
    }

    fn receive_command_mask(self) -> u32 {
        match self {
            Self::Nrf9160 => 0x00000004,
            Self::Nrf9151 => 0x0000FFFF,
        }
    }

    fn receive_data_mask(self) -> u32 {
        match self {
            Self::Nrf9160 => 0x00000010,
            Self::Nrf9151 => 0x0000FFFF,
        }
    }

    /// Whether UICR writes must go through a flash algorithm (true on
    /// nRF9151, false on nRF9160 where UICR is plain flash).
    pub fn uicr_requires_flash_algorithm(self) -> bool {
        matches!(self, Self::Nrf9151)
    }
}

impl core::str::FromStr for TargetProfile {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("nrf9160") || value.eq_ignore_ascii_case("9160") {
            Ok(Self::Nrf9160)
        } else if value.eq_ignore_ascii_case("nrf9151") || value.eq_ignore_ascii_case("9151") {
            Ok(Self::Nrf9151)
        } else {
            Err("supported values are: nrf9160, nrf9151")
        }
    }
}

impl core::fmt::Display for TargetProfile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Nrf9160 => f.write_str("nrf9160"),
            Self::Nrf9151 => f.write_str("nrf9151"),
        }
    }
}

// ---------------------------------------------------------------------------
// I/O traits
// ---------------------------------------------------------------------------

/// 32-bit and byte-granular memory access to the application core's RAM
/// and memory-mapped peripherals.
///
/// On a PC this is typically backed by probe-rs `Core` operations. On the
/// device itself it can be a direct `core::ptr::write_volatile`-based
/// implementation.
pub trait ModemMemory {
    type Error;

    fn read_word_32(&mut self, addr: u32) -> Result<u32, Self::Error>;
    fn write_word_32(&mut self, addr: u32, value: u32) -> Result<(), Self::Error>;
    fn write_bytes(&mut self, addr: u32, data: &[u8]) -> Result<(), Self::Error>;
}

/// Monotonic millisecond clock used by [`ProtocolEngine::wait_and_ack`] for
/// timeouts. Only differences between successive calls are observed.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ProtocolError<E> {
    Memory(E),
    Timeout,
    Nack(u32),
    Fault,
    BufferTooLarge,
}

impl<E: core::fmt::Display> core::fmt::Display for ProtocolError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProtocolError::Memory(e) => write!(f, "memory access error: {}", e),
            ProtocolError::Timeout => f.write_str("timeout waiting for ACK or NACK response"),
            ProtocolError::Nack(code) => write!(f, "NACK response, code {:08X}", code),
            ProtocolError::Fault => f.write_str("modem triggered FAULT_EVENT"),
            ProtocolError::BufferTooLarge => f.write_str("chunk exceeds buffer size"),
        }
    }
}

#[cfg(feature = "std")]
impl<E: core::fmt::Debug + core::fmt::Display> std::error::Error for ProtocolError<E> {}

// ---------------------------------------------------------------------------
// HFXO helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HfxoStatus {
    pub hfxosr_set: bool,
    pub hfxocnt_set: bool,
}

impl HfxoStatus {
    pub fn fully_programmed(&self) -> bool {
        self.hfxosr_set && self.hfxocnt_set
    }
}

/// Recommended UICR HFXO trim values: `(HFXOSR, HFXOCNT)`.
pub const fn hfxo_defaults() -> (u32, u32) {
    (UICR_HFXOSR_DEFAULT, UICR_HFXOCNT_DEFAULT)
}

/// UICR addresses for the HFXO trim registers: `(HFXOSR_addr, HFXOCNT_addr)`.
pub const fn hfxo_addresses() -> (u32, u32) {
    (UICR_HFXOSR_ADDR, UICR_HFXOCNT_ADDR)
}

// ---------------------------------------------------------------------------
// Protocol engine
// ---------------------------------------------------------------------------

/// Stateful driver for the nRF91 IPC DFU protocol.
///
/// The engine itself holds no I/O resources: each operation takes a
/// [`ModemMemory`] and (where relevant) a [`Clock`] by reference, so it
/// composes naturally with either a probe-rs-backed adapter on a host or
/// direct memory access on-device.
pub struct ProtocolEngine {
    target: TargetProfile,
    pipelined: bool,
    response_timeout_ms: u64,
}

impl ProtocolEngine {
    pub const fn new(target: TargetProfile) -> Self {
        Self {
            target,
            pipelined: false,
            response_timeout_ms: DEFAULT_RESPONSE_TIMEOUT_MS,
        }
    }

    pub fn target(&self) -> TargetProfile {
        self.target
    }

    pub fn set_pipelined(&mut self, pipelined: bool) {
        self.pipelined = pipelined;
    }

    pub fn is_pipelined(&self) -> bool {
        self.pipelined
    }

    pub fn set_response_timeout_ms(&mut self, ms: u64) {
        self.response_timeout_ms = ms;
    }

    /// Maximum payload size accepted by [`write_chunk`](Self::write_chunk)
    /// for the current pipelining mode.
    pub fn max_chunk_size(&self) -> usize {
        if self.pipelined {
            IPC_PIPELINED_MAX_BUFFER_SIZE
        } else {
            IPC_MAX_BUFFER_SIZE
        }
    }

    /// Address in app-core RAM where chunk data for `bank` is staged.
    pub fn data_buffer_address(&self, bank: u8) -> u32 {
        if self.pipelined {
            DATA_BUFFER_PIPELINED_BASE + (bank as u32) * (IPC_PIPELINED_MAX_BUFFER_SIZE as u32)
        } else {
            DATA_BUFFER_NON_PIPELINED
        }
    }

    /// Returns the modem ROM key digest written by the bootrom at
    /// `0x20000010`, byte-swapped to match Nordic's display order.
    ///
    /// The 7-most-significant hex characters (top-down) are what the
    /// `<digest>.ipc_dfu.signed_*.ihex` filename in legacy modem firmware
    /// zips encodes.
    pub fn read_key_digest<M: ModemMemory>(
        &self,
        mem: &mut M,
    ) -> Result<u32, ProtocolError<M::Error>> {
        let raw = mem
            .read_word_32(MAILBOX_ARG0_ADDR)
            .map_err(ProtocolError::Memory)?;
        Ok(raw.swap_bytes())
    }

    /// Reads UICR HFXO trim registers and reports whether they need
    /// programming.
    pub fn hfxo_status<M: ModemMemory>(
        &self,
        mem: &mut M,
    ) -> Result<HfxoStatus, ProtocolError<M::Error>> {
        let hfxosr = mem
            .read_word_32(UICR_HFXOSR_ADDR)
            .map_err(ProtocolError::Memory)?;
        let hfxocnt = mem
            .read_word_32(UICR_HFXOCNT_ADDR)
            .map_err(ProtocolError::Memory)?;
        Ok(HfxoStatus {
            hfxosr_set: hfxosr != UICR_ERASED,
            hfxocnt_set: hfxocnt != UICR_ERASED,
        })
    }

    /// Writes the default HFXO trim values directly to UICR.
    ///
    /// Only usable on parts where UICR is plain (non-protected) flash —
    /// i.e. nRF9160. For nRF9151 the caller must perform the write through
    /// the chip's flash algorithm (see [`TargetProfile::uicr_requires_flash_algorithm`]).
    pub fn write_hfxo_defaults_raw<M: ModemMemory>(
        &self,
        mem: &mut M,
    ) -> Result<(), ProtocolError<M::Error>> {
        mem.write_word_32(UICR_HFXOSR_ADDR, UICR_HFXOSR_DEFAULT)
            .map_err(ProtocolError::Memory)?;
        mem.write_word_32(UICR_HFXOCNT_ADDR, UICR_HFXOCNT_DEFAULT)
            .map_err(ProtocolError::Memory)?;
        Ok(())
    }

    /// Programs the IPC peripheral, SPU RAM permissions, mailbox header,
    /// and pulses the modem reset line.
    ///
    /// The caller is responsible for having halted the application core
    /// and (if needed) programmed UICR HFXO before calling this.
    pub fn setup<M: ModemMemory>(&self, mem: &mut M) -> Result<(), ProtocolError<M::Error>> {
        let m = |r: Result<(), M::Error>| r.map_err(ProtocolError::Memory);

        m(mem.write_word_32(IPC_ROUTE_ADDR, IPC_ROUTE_VALUE))?;
        m(mem.write_word_32(IPC_SEND_CNF0_ADDR, IPC_SEND_CNF0_VALUE))?;
        m(mem.write_word_32(IPC_SEND_CNF2_ADDR, IPC_SEND_CNF2_VALUE))?;
        m(mem.write_word_32(IPC_GPMEM0_ADDR, IPC_GPMEM0_VALUE))?;
        m(mem.write_word_32(IPC_GPMEM1_ADDR, IPC_GPMEM1_VALUE))?;
        m(mem.write_word_32(IPC_RECEIVE_FAULT_ADDR, IPC_RECEIVE_FAULT_MASK))?;
        m(mem.write_word_32(
            IPC_RECEIVE_COMMAND_ADDR,
            self.target.receive_command_mask(),
        ))?;
        m(mem.write_word_32(IPC_RECEIVE_DATA_ADDR, self.target.receive_data_mask()))?;

        for n in 0..SPU_RAMREGION_COUNT {
            m(mem.write_word_32(SPU_RAMREGION_BASE + (n * 4), SPU_RAMREGION_PERM))?;
        }

        m(mem.write_word_32(MAILBOX_HEADER_ADDR, MAILBOX_HEADER_MAGIC))?;
        m(mem.write_word_32(MAILBOX_HEADER_PTR_ADDR, MAILBOX_HEADER_PTR_VALUE))?;
        m(mem.write_word_32(MAILBOX_HEADER_SIZE_ADDR, MAILBOX_HEADER_SIZE_VALUE))?;

        // Pulse modem reset
        m(mem.write_word_32(MODEM_RESET_FORCEOFF, 0))?;
        m(mem.write_word_32(MODEM_RESET_NETWORK_FORCEOFF, 1))?;
        m(mem.write_word_32(MODEM_RESET_FORCEOFF, 1))?;
        m(mem.write_word_32(MODEM_RESET_NETWORK_FORCEOFF, 0))?;
        m(mem.write_word_32(MODEM_RESET_FORCEOFF, 0))?;

        Ok(())
    }

    /// Triggers the IPC `SEND[0]` task and waits for the loader to ACK,
    /// indicating it is ready to accept commands.
    pub fn start_loader<M: ModemMemory, C: Clock>(
        &self,
        mem: &mut M,
        clock: &C,
    ) -> Result<(), ProtocolError<M::Error>> {
        mem.write_word_32(IPC_TASKS_SEND0, 1)
            .map_err(ProtocolError::Memory)?;
        self.wait_and_ack(mem, clock)
    }

    /// Writes one chunk of firmware data to modem flash and waits for ACK.
    ///
    /// `bank` selects the double-buffer bank (0 or 1) in pipelined mode
    /// and must be 0 in non-pipelined mode. The caller is expected to
    /// alternate `bank` across successive calls to overlap the host upload
    /// with modem flash writes.
    pub fn write_chunk<M: ModemMemory, C: Clock>(
        &self,
        mem: &mut M,
        clock: &C,
        addr: u32,
        data: &[u8],
        bank: u8,
    ) -> Result<(), ProtocolError<M::Error>> {
        if data.len() > self.max_chunk_size() {
            return Err(ProtocolError::BufferTooLarge);
        }

        let buf_addr = self.data_buffer_address(bank);
        mem.write_bytes(buf_addr, data)
            .map_err(ProtocolError::Memory)?;

        mem.write_word_32(MAILBOX_ARG0_ADDR, addr)
            .map_err(ProtocolError::Memory)?;
        mem.write_word_32(MAILBOX_ARG1_ADDR, data.len() as u32)
            .map_err(ProtocolError::Memory)?;

        if self.pipelined {
            let buffer_offset = (bank as u32) * (IPC_PIPELINED_MAX_BUFFER_SIZE as u32);
            mem.write_word_32(MAILBOX_ARG2_ADDR, buffer_offset)
                .map_err(ProtocolError::Memory)?;
            mem.write_word_32(MAILBOX_COMMAND_ADDR, CMD_PIPELINE_WRITE)
                .map_err(ProtocolError::Memory)?;
        } else {
            mem.write_word_32(MAILBOX_COMMAND_ADDR, CMD_WRITE)
                .map_err(ProtocolError::Memory)?;
        }

        mem.write_word_32(IPC_TASKS_SEND0, 1)
            .map_err(ProtocolError::Memory)?;

        self.wait_and_ack(mem, clock)
    }

    /// Issues `VERIFY` for the listed `(address, length)` ranges and
    /// returns the SHA-256 the modem computed over the concatenation of
    /// their flash contents in ascending address order.
    pub fn verify<M: ModemMemory, C: Clock>(
        &self,
        mem: &mut M,
        clock: &C,
        ranges: &[(u32, u32)],
    ) -> Result<[u8; 32], ProtocolError<M::Error>> {
        mem.write_word_32(MAILBOX_ARG0_ADDR, ranges.len() as u32)
            .map_err(ProtocolError::Memory)?;

        for (i, &(start, len)) in ranges.iter().enumerate() {
            let base = MAILBOX_ARG1_ADDR + (8 * i as u32);
            mem.write_word_32(base, start)
                .map_err(ProtocolError::Memory)?;
            mem.write_word_32(base + 4, len)
                .map_err(ProtocolError::Memory)?;
        }

        mem.write_word_32(MAILBOX_COMMAND_ADDR, CMD_VERIFY)
            .map_err(ProtocolError::Memory)?;
        mem.write_word_32(IPC_TASKS_SEND0, 1)
            .map_err(ProtocolError::Memory)?;

        self.wait_and_ack(mem, clock)?;

        let mut digest = [0u8; 32];
        for i in 0..8 {
            let word = mem
                .read_word_32(MAILBOX_ARG0_ADDR + (4 * i as u32))
                .map_err(ProtocolError::Memory)?;
            digest[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        Ok(digest)
    }

    /// Polls the IPC `EVENTS_RECEIVE` registers until one fires or the
    /// configured timeout expires, clears them, and inspects the response
    /// word at `0x2000000C`.
    ///
    /// Returns `Ok(())` on ACK, `Err(Nack)` on NACK, `Err(Fault)` if the
    /// fault channel fired, and `Err(Timeout)` if no event arrived in time.
    pub fn wait_and_ack<M: ModemMemory, C: Clock>(
        &self,
        mem: &mut M,
        clock: &C,
    ) -> Result<(), ProtocolError<M::Error>> {
        let start = clock.now_ms();
        let mut fault = false;

        loop {
            if clock.now_ms().wrapping_sub(start) > self.response_timeout_ms {
                return Err(ProtocolError::Timeout);
            }

            if mem
                .read_word_32(IPC_EVENT_FAULT)
                .map_err(ProtocolError::Memory)?
                != 0
            {
                fault = true;
                break;
            }
            if mem
                .read_word_32(IPC_EVENT_COMMAND)
                .map_err(ProtocolError::Memory)?
                != 0
            {
                break;
            }
            if mem
                .read_word_32(IPC_EVENT_DATA)
                .map_err(ProtocolError::Memory)?
                != 0
            {
                break;
            }
        }

        mem.write_word_32(IPC_EVENT_FAULT, 0)
            .map_err(ProtocolError::Memory)?;
        mem.write_word_32(IPC_EVENT_COMMAND, 0)
            .map_err(ProtocolError::Memory)?;
        mem.write_word_32(IPC_EVENT_DATA, 0)
            .map_err(ProtocolError::Memory)?;

        let response = mem
            .read_word_32(MAILBOX_COMMAND_ADDR)
            .map_err(ProtocolError::Memory)?;

        if (response & RESPONSE_PREFIX_MASK) == NACK_PREFIX {
            return Err(ProtocolError::Nack(response & RESPONSE_PAYLOAD_MASK));
        }

        // ACK_PREFIX is informational; an unknown prefix is silently
        // accepted as success because some loader versions don't set the
        // response word on simple completions.
        let _ = ACK_PREFIX;

        if fault {
            return Err(ProtocolError::Fault);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_profile_parsing() {
        assert_eq!(
            "nrf9160".parse::<TargetProfile>().unwrap(),
            TargetProfile::Nrf9160
        );
        assert_eq!(
            "9151".parse::<TargetProfile>().unwrap(),
            TargetProfile::Nrf9151
        );
        assert!("nrf9999".parse::<TargetProfile>().is_err());
    }

    #[test]
    fn buffer_addressing() {
        let mut engine = ProtocolEngine::new(TargetProfile::Nrf9151);
        assert_eq!(engine.max_chunk_size(), IPC_MAX_BUFFER_SIZE);
        assert_eq!(engine.data_buffer_address(0), DATA_BUFFER_NON_PIPELINED);

        engine.set_pipelined(true);
        assert_eq!(engine.max_chunk_size(), IPC_PIPELINED_MAX_BUFFER_SIZE);
        assert_eq!(engine.data_buffer_address(0), DATA_BUFFER_PIPELINED_BASE);
        assert_eq!(
            engine.data_buffer_address(1),
            DATA_BUFFER_PIPELINED_BASE + IPC_PIPELINED_MAX_BUFFER_SIZE as u32
        );
    }

    #[test]
    fn hfxo_defaults_match_legacy() {
        let (sr, cnt) = hfxo_defaults();
        assert_eq!(sr, 0x0000000E);
        assert_eq!(cnt, 0x00000020);
    }
}
