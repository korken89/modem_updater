// Copyright 2025 Jared Wolff
//
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Apache-2.0 applies with the trademark modification described in
// LICENSE-APACHE.

//! PC-side wrappers around the `protocol` module.
//!
//! This module bundles everything that depends on the host environment:
//! probe-rs for SWD memory access and flashing, Nordic's `.zip` package
//! layout, and Intel HEX parsing. The pure protocol layer lives in
//! [`crate::protocol`] and remains usable from `no_std` contexts.

use bin_file::BinFile;
use probe_rs::flashing::{self};
use probe_rs::{MemoryInterface, Session};
use regex::Regex;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use thiserror::Error;
use zip::read::ZipArchive;

use crate::protocol::{
    self, hfxo_defaults, Clock, ModemMemory, ProtocolEngine, ProtocolError, TargetProfile,
    DEFAULT_RESPONSE_TIMEOUT_MS, PREPARE_RESPONSE_TIMEOUT_MS,
};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum ModemUpdateError {
    #[error("{0}")]
    ProbeError(#[from] probe_rs::Error),
    #[error("{0}")]
    FileDownloadError(#[from] flashing::FileDownloadError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Zip archive error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Hex file error: {0}")]
    HexParse(#[from] bin_file::Error),
    /// Timeout while waiting for ACK or NACK response.
    #[error("Timeout waiting for ACK or NACK response")]
    Timeout,
    /// NACK response received from the modem loader.
    #[error("NACK response, code {0:08X}")]
    NACKResponseError(u32),
    /// The modem signalled a fault during a command.
    #[error("Modem triggered FAULT_EVENT")]
    FaultEventError,
    #[error("Unable to find modem firmware loader")]
    LoaderNotFound,
    #[error("No segments found!")]
    NoSegmentsFound,
    #[error("No digest found!")]
    NoDigestFound,
    #[error("Chunk buffer too large for current pipelining mode")]
    BufferTooLarge,
}

impl From<ProtocolError<probe_rs::Error>> for ModemUpdateError {
    fn from(value: ProtocolError<probe_rs::Error>) -> Self {
        match value {
            ProtocolError::Memory(e) => Self::ProbeError(e),
            ProtocolError::Timeout => Self::Timeout,
            ProtocolError::Nack(code) => Self::NACKResponseError(code),
            ProtocolError::Fault => Self::FaultEventError,
            ProtocolError::BufferTooLarge => Self::BufferTooLarge,
        }
    }
}

// ---------------------------------------------------------------------------
// Adapters: probe-rs `Core` -> `ModemMemory`, `Instant` -> `Clock`
// ---------------------------------------------------------------------------

/// `ModemMemory` adapter over a probe-rs `Core`.
pub struct ProbeRsMemory<'a>(pub probe_rs::Core<'a>);

impl<'a> ModemMemory for ProbeRsMemory<'a> {
    type Error = probe_rs::Error;

    fn read_word_32(&mut self, addr: u32) -> Result<u32, Self::Error> {
        self.0.read_word_32(addr as u64)
    }

    fn write_word_32(&mut self, addr: u32, value: u32) -> Result<(), Self::Error> {
        self.0.write_word_32(addr as u64, value)
    }

    fn write_bytes(&mut self, addr: u32, data: &[u8]) -> Result<(), Self::Error> {
        self.0.write(addr as u64, data)
    }
}

/// Monotonic clock backed by `std::time::Instant`.
#[derive(Debug, Clone, Copy)]
pub struct StdClock {
    start: Instant,
}

impl StdClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for StdClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for StdClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

// ---------------------------------------------------------------------------
// ModemUpdater
// ---------------------------------------------------------------------------

type ProgressCallback = Box<dyn FnMut(u64, u64) + Send + 'static>;
type StatusCallback = Box<dyn FnMut(&str) + Send + 'static>;

/// PC-side driver that orchestrates a full modem firmware update from a
/// Nordic-supplied `.zip` package.
///
/// The driver can be used in one of two ways:
///
/// * **One-shot:** call [`verify`](Self::verify) or
///   [`program_and_verify`](Self::program_and_verify) and they will run the
///   full pipeline (setup → load loader → optionally program → verify).
/// * **Phased:** call [`prepare`](Self::prepare),
///   [`program_segments`](Self::program_segments) and
///   [`verify_loaded`](Self::verify_loaded) yourself. This lets an external
///   orchestrator interleave phases across multiple probes (e.g. serial
///   prepare + parallel program/verify when fan-out programming a hub).
pub struct ModemUpdater<'a> {
    session: &'a mut Session,
    engine: ProtocolEngine,
    clock: StdClock,
    segments: BTreeMap<u32, PathBuf>,
    firmware_update_digest: Option<[u8; 32]>,
    /// Owned temp dir backing the `segments` PathBufs after [`prepare`].
    /// Held here so it outlives [`program_segments`] / [`verify_loaded`].
    temp_dir: Option<TempDir>,
    progress_callback: Option<ProgressCallback>,
    status_callback: Option<StatusCallback>,
    progress_total: u64,
    progress_current: u64,
}

impl<'a> ModemUpdater<'a> {
    pub fn new(session: &'a mut Session) -> Self {
        Self::new_with_target(session, TargetProfile::default())
    }

    pub fn new_with_target(session: &'a mut Session, target_profile: TargetProfile) -> Self {
        Self {
            session,
            engine: ProtocolEngine::new(target_profile),
            clock: StdClock::new(),
            segments: BTreeMap::new(),
            firmware_update_digest: None,
            temp_dir: None,
            progress_callback: None,
            status_callback: None,
            progress_total: 0,
            progress_current: 0,
        }
    }

    pub fn set_progress_callback<F>(&mut self, callback: F)
    where
        F: FnMut(u64, u64) + Send + 'static,
    {
        self.progress_callback = Some(Box::new(callback));
    }

    pub fn set_status_callback<F>(&mut self, callback: F)
    where
        F: FnMut(&str) + Send + 'static,
    {
        self.status_callback = Some(Box::new(callback));
    }

    fn emit_progress(&mut self) {
        if let Some(cb) = self.progress_callback.as_mut() {
            cb(self.progress_current, self.progress_total);
        }
    }

    fn emit_status(&mut self, status: &str) {
        if let Some(cb) = self.status_callback.as_mut() {
            cb(status);
        }
    }

    fn increment_progress(&mut self, bytes: usize) {
        if self.progress_total == 0 {
            return;
        }
        self.progress_current = (self.progress_current + bytes as u64).min(self.progress_total);
        self.emit_progress();
    }

    fn calculate_total_segment_bytes(&self) -> Result<u64, ModemUpdateError> {
        let mut total = 0u64;
        for path in self.segments.values() {
            let hex = BinFile::from_file(path)?;
            for segment in hex.segments() {
                let (_, data) = segment.get_tuple();
                total += data.len() as u64;
            }
        }
        Ok(total)
    }

    /// Run the full verify pipeline (prepare → verify_loaded).
    pub fn verify(&mut self, mfw_zip: impl AsRef<Path>) -> Result<bool, ModemUpdateError> {
        self.prepare(mfw_zip)?;
        self.verify_loaded()
    }

    /// Run the full program+verify pipeline (prepare → program_segments →
    /// verify_loaded).
    pub fn program_and_verify(
        &mut self,
        mfw_zip: impl AsRef<Path>,
    ) -> Result<bool, ModemUpdateError> {
        self.prepare(mfw_zip)?;
        self.program_segments()?;
        self.verify_loaded()
    }

    /// Phase 1: bring the device up, extract `mfw_zip`, program the modem
    /// firmware loader and start it. After this call the chip is ready for
    /// [`program_segments`](Self::program_segments) and
    /// [`verify_loaded`](Self::verify_loaded).
    pub fn prepare(&mut self, mfw_zip: impl AsRef<Path>) -> Result<(), ModemUpdateError> {
        // Setup / loader-boot / digest-read ACKs are sub-second on healthy
        // boards; use a tight timeout so a wedged modem trips immediately
        // and the caller's recovery path kicks in. Restore the longer
        // default for the segment-write / verify phases that follow.
        self.engine
            .set_response_timeout_ms(PREPARE_RESPONSE_TIMEOUT_MS);
        let result = (|| -> Result<(), ModemUpdateError> {
            self.emit_status("Preparing device");
            self.setup_device()?;
            self.emit_status("Loading firmware package");
            self.process_zip_file(mfw_zip.as_ref())?;
            Ok(())
        })();
        self.engine
            .set_response_timeout_ms(DEFAULT_RESPONSE_TIMEOUT_MS);
        result
    }

    /// Phase 2: write all firmware segments to the modem. Requires that
    /// [`prepare`](Self::prepare) has run on this updater.
    pub fn program_segments(&mut self) -> Result<(), ModemUpdateError> {
        let total_bytes = self.calculate_total_segment_bytes()?;
        if total_bytes > 0 {
            self.progress_total = total_bytes;
            self.progress_current = 0;
            self.emit_progress();
        }

        self.emit_status("Programming modem firmware");
        log::info!("Programming modem firmware..");

        for segment in self.segments.values().cloned().collect::<Vec<PathBuf>>() {
            self.program_segment(&segment)?;
        }

        log::info!("Modem firmware programmed.");

        if self.progress_total > 0 {
            self.progress_current = self.progress_total;
            self.emit_progress();
        }

        Ok(())
    }

    /// Phase 3: verify the firmware digest and reset the chip. Requires that
    /// [`prepare`](Self::prepare) (and, for the program path,
    /// [`program_segments`](Self::program_segments)) has run.
    pub fn verify_loaded(&mut self) -> Result<bool, ModemUpdateError> {
        self.emit_status("Verifying modem firmware");
        log::info!("Verifying modem firmware.");
        let verified = match self.run_verify() {
            Ok(v) => {
                if v {
                    log::info!("Modem firmware verified.");
                } else {
                    log::info!("Modem firmware verification failed!");
                }
                v
            }
            Err(e) => {
                log::error!("Modem firmware verification failed! Error: {}", e);
                return Err(e);
            }
        };

        self.session.core(0)?.reset()?;
        Ok(verified)
    }

    // ---------------- internals ----------------

    fn setup_device(&mut self) -> Result<(), ModemUpdateError> {
        self.emit_status("Configuring device");
        self.ensure_hfxo_config()?;

        let mut mem = ProbeRsMemory(self.session.core(0)?);
        mem.0.reset_and_halt(Duration::from_secs(5))?;
        self.engine.setup(&mut mem)?;
        Ok(())
    }

    fn ensure_hfxo_config(&mut self) -> Result<(), ModemUpdateError> {
        let target = self.engine.target();

        let status = {
            let mut mem = ProbeRsMemory(self.session.core(0)?);
            mem.0.reset_and_halt(Duration::from_secs(5))?;
            self.engine.hfxo_status(&mut mem)?
        };

        if status.fully_programmed() {
            return Ok(());
        }

        let (hfxosr_default, hfxocnt_default) = hfxo_defaults();
        let (hfxosr_addr, hfxocnt_addr) = protocol::hfxo_addresses();

        if !target.uicr_requires_flash_algorithm() {
            self.emit_status("Programming UICR");
            let mut mem = ProbeRsMemory(self.session.core(0)?);
            mem.0.reset_and_halt(Duration::from_secs(5))?;
            self.engine.write_hfxo_defaults_raw(&mut mem)?;
            return Ok(());
        }

        // nRF9151: UICR sits in protected flash and must go through the
        // chip's flash algorithm. Read the existing values back so we don't
        // overwrite a half-programmed UICR.
        let (existing_hfxosr, existing_hfxocnt) = {
            let mut core = self.session.core(0)?;
            (
                core.read_word_32(hfxosr_addr as u64)?,
                core.read_word_32(hfxocnt_addr as u64)?,
            )
        };

        let hfxosr_value = if status.hfxosr_set {
            existing_hfxosr
        } else {
            hfxosr_default
        };
        let hfxocnt_value = if status.hfxocnt_set {
            existing_hfxocnt
        } else {
            hfxocnt_default
        };

        let mut uicr_bin: Vec<u8> = Vec::with_capacity(8);
        uicr_bin.extend_from_slice(&hfxosr_value.to_le_bytes());
        uicr_bin.extend_from_slice(&hfxocnt_value.to_le_bytes());

        let uicr_tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(uicr_tmp.path(), &uicr_bin)?;

        self.emit_status("Programming UICR");
        log::info!(
            "Programming UICR HFXO settings for {} via flash algorithm",
            target
        );
        flashing::download_file(
            self.session,
            uicr_tmp.path(),
            flashing::Format::Bin(flashing::BinOptions {
                base_address: Some(hfxosr_addr as u64),
                skip: 0,
            }),
        )?;

        Ok(())
    }

    fn process_zip_file(&mut self, mfw_zip: &Path) -> Result<(), ModemUpdateError> {
        self.emit_status("Extracting firmware package");
        let temp_dir = TempDir::new()?;
        let file = File::open(mfw_zip)?;
        ZipArchive::new(file)?.extract(temp_dir.path())?;

        let digest_id = self.read_key_digest_string()?;

        let loader_re =
            Regex::new(r"(?:\.ipc_dfu\.signed_|ipc-dfu_nrf91x1_)(\d+)\.(\d+)\.(\d+)\.ihex")
                .expect("static regex literal");
        let segment_re = Regex::new(r"firmware\.update\.image\.segments\.(\d+).hex")
            .expect("static regex literal");
        let digest_prefix = format!("{}.ipc_dfu.signed_", digest_id);

        // Nordic packages can ship the loader twice: a legacy digest-prefixed
        // form (`<digest>.ipc_dfu.signed_X.Y.Z.ihex`) and a newer
        // `ipc-dfu_nrf91x1_X.Y.Z.ihex`. Both refer to the same loader; pick
        // the new form when available so we only program/log once.
        let mut new_style_loader: Option<PathBuf> = None;
        let mut legacy_loader: Option<PathBuf> = None;

        for entry in std::fs::read_dir(&temp_dir)? {
            let entry = entry?;
            let os_name = entry.file_name();
            let Some(file_name) = os_name.to_str() else {
                log::debug!("Skipping non-UTF-8 file name in package");
                continue;
            };
            log::debug!("Processing file: {}", file_name);

            if file_name.starts_with("ipc-dfu_nrf91x1_") {
                new_style_loader = Some(temp_dir.path().join(file_name));
            } else if file_name.starts_with(&digest_prefix) {
                legacy_loader = Some(temp_dir.path().join(file_name));
            } else if let Some(c) = segment_re.captures(file_name) {
                let segment: u32 = c[1].parse().expect("\\d+ in regex");
                log::info!("Inserting segment: {}:{}", segment, file_name);
                self.segments
                    .insert(segment, temp_dir.path().join(file_name));
            }
        }

        let modem_firmware_loader = new_style_loader
            .or(legacy_loader)
            .ok_or(ModemUpdateError::LoaderNotFound)?;

        let loader_name = modem_firmware_loader
            .file_name()
            .and_then(|n| n.to_str())
            .expect("loader path built from a UTF-8 file name above");

        if let Some(c) = loader_re.captures(loader_name) {
            let major: u32 = c[1].parse().expect("\\d+ in regex");
            let minor: u32 = c[2].parse().expect("\\d+ in regex");
            let patch: u32 = c[3].parse().expect("\\d+ in regex");

            log::info!(
                "modem_firmware_loader version: {}.{}.{}",
                major,
                minor,
                patch
            );

            // Pipelined loader from > 1.1.2 onwards.
            if (major, minor, patch) > (1, 1, 2) {
                log::info!("Using pipelined loader");
                self.engine.set_pipelined(true);
            }
        } else {
            log::error!("Unable to parse loader file name: {}", loader_name);
        }

        if self.segments.is_empty() {
            return Err(ModemUpdateError::NoSegmentsFound);
        }

        let digest_path = temp_dir.path().join("firmware.update.image.digest.txt");
        log::debug!("Opening {}", digest_path.display());

        if let Ok(f) = std::fs::File::open(&digest_path) {
            log::info!("Parsing segment digests");

            let reader = std::io::BufReader::new(f);
            let m = Regex::new(r"SHA256 of all ranges in ascending address order:\s*(\w{64})")
                .expect("static regex literal");

            for line in reader.lines().map_while(Result::ok) {
                if let Some(c) = m.captures(&line) {
                    let digest_hex = c.get(1).expect("captured by regex").as_str();
                    log::info!("Firmware digest: {}", digest_hex);
                    self.firmware_update_digest = Some(parse_sha256_hex(digest_hex));
                    break;
                }
            }

            if self.firmware_update_digest.is_none() {
                return Err(ModemUpdateError::NoDigestFound);
            }
        }

        log::info!(
            "Programming modem firmware loader: {}",
            modem_firmware_loader.display()
        );

        self.emit_status("Programming modem loader");
        flashing::download_file(self.session, modem_firmware_loader, flashing::Format::Hex)?;

        self.emit_status("Starting modem loader");
        let mut mem = ProbeRsMemory(self.session.core(0)?);
        self.engine.start_loader(&mut mem, &self.clock)?;

        log::info!("modem_firmware_loader started!");
        // Keep the extracted package alive: `self.segments` and the digest
        // file paths point into it, and program_segments / verify_loaded
        // re-read them from disk.
        self.temp_dir = Some(temp_dir);
        Ok(())
    }

    fn read_key_digest_string(&mut self) -> Result<String, ModemUpdateError> {
        let core = self.session.core(0)?;
        let mut mem = ProbeRsMemory(core);
        // Loader response from the previous setup_device step is waiting;
        // drain it.
        self.engine.wait_and_ack(&mut mem, &self.clock)?;
        let digest = self.engine.read_key_digest(&mut mem)?;
        let mut s = format!("{:08X}", digest);
        s.truncate(7);
        Ok(s)
    }

    fn program_segment(&mut self, segment: &Path) -> Result<(), ModemUpdateError> {
        log::info!("Programming segment: {}", segment.display());

        let hex = BinFile::from_file(segment)?;
        let bufsz = self.engine.max_chunk_size();
        let chunks = hex.segments().chunks(Some(bufsz), None)?;

        for (i, (addr, data)) in chunks.into_iter().enumerate() {
            log::info!("Reading segment: {} with size {}", addr, data.len());

            let bank = if self.engine.is_pipelined() {
                (i % 2) as u8
            } else {
                0
            };

            let core = self.session.core(0)?;
            let mut mem = ProbeRsMemory(core);
            self.engine
                .write_chunk(&mut mem, &self.clock, addr as u32, &data, bank)?;
            drop(mem);

            self.increment_progress(data.len());

            if self.engine.is_pipelined() {
                log::info!("Wrote chunk: {}:{} for bank {}", i, addr, bank);
            }
        }

        Ok(())
    }

    fn run_verify(&mut self) -> Result<bool, ModemUpdateError> {
        let mut ranges_to_verify: Vec<(u32, u32)> = Vec::new();
        for s in self.segments.values() {
            let hex = BinFile::from_file(s)?;
            for s in hex.segments() {
                let (addr, data) = s.get_tuple();
                if addr < 0x1000000 {
                    log::info!("Verifying segment: {}", addr);
                    ranges_to_verify.push((addr as u32, data.len() as u32));
                }
            }
        }

        let expected = self
            .firmware_update_digest
            .ok_or(ModemUpdateError::NoDigestFound)?;

        let mut mem = ProbeRsMemory(self.session.core(0)?);
        let digest = self
            .engine
            .verify(&mut mem, &self.clock, &ranges_to_verify)?;

        if digest != expected {
            log::info!(
                "checksum mismatch: {} != {}",
                hex_string(&digest),
                hex_string(&expected)
            );
            Ok(false)
        } else {
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_sha256_hex(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(s.as_bytes()[i * 2]);
        let lo = hex_nibble(s.as_bytes()[i * 2 + 1]);
        *byte = (hi << 4) | lo;
    }
    out
}

fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

fn hex_string(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{:02X}", b);
    }
    s
}
