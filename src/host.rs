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
    Clock, HfxoConfig, IPC_CHUNK_ALIGNMENT, ModemMemory, ProgrammedHfxo, ProtocolEngine,
    ProtocolError, TargetProfile,
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
    /// Timeout while waiting for a target operation.
    #[error("Target operation timed out")]
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
    #[error("Invalid chunk bank {0}")]
    InvalidBank(u8),
    #[error("Programmed UICR HFXO words differ from --hfxosrc/--hfxocnt")]
    HfxoMismatch,
    #[error("Chunk address {0:08X} is not 8 KiB aligned")]
    MisalignedAddress(u32),
}

impl From<ProtocolError<probe_rs::Error>> for ModemUpdateError {
    fn from(value: ProtocolError<probe_rs::Error>) -> Self {
        match value {
            ProtocolError::Memory(e) => Self::ProbeError(e),
            ProtocolError::Timeout => Self::Timeout,
            ProtocolError::Nack(code) => Self::NACKResponseError(code),
            ProtocolError::Fault => Self::FaultEventError,
            ProtocolError::BufferTooLarge => Self::BufferTooLarge,
            ProtocolError::InvalidBank(bank) => Self::InvalidBank(bank),
            ProtocolError::MisalignedAddress(addr) => Self::MisalignedAddress(addr),
            ProtocolError::HfxoMismatch => Self::HfxoMismatch,
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LoaderVersion(u32, u32, u32);

#[derive(Debug)]
struct LoaderCandidate {
    path: PathBuf,
    version: LoaderVersion,
}

impl LoaderCandidate {
    fn is_preferred_to(&self, other: &Self) -> bool {
        self.version > other.version
    }
}

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
    /// Board HFXO values, or `None` for Nordic's development kit values.
    hfxo_config: Option<HfxoConfig>,
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
            hfxo_config: None,
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

    /// Selects the board's UICR HFXO values. Erased words get them, and programming fails if a
    /// programmed word holds another value.
    pub fn set_hfxo_config(&mut self, config: HfxoConfig) {
        self.hfxo_config = Some(config);
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
        self.emit_status("Preparing device");
        self.setup_device()?;
        self.emit_status("Loading firmware package");
        self.process_zip_file(mfw_zip.as_ref())
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
        let status = {
            let mut mem = ProbeRsMemory(self.session.core(0)?);
            mem.0.reset_and_halt(Duration::from_secs(5))?;
            self.engine.hfxo_status(&mut mem)?
        };

        let (config, programmed) = match self.hfxo_config {
            Some(config) => (config, ProgrammedHfxo::MustMatch),
            None => (HfxoConfig::default(), ProgrammedHfxo::Keep),
        };
        if status.fully_programmed() && programmed == ProgrammedHfxo::Keep {
            return Ok(());
        }

        self.emit_status("Programming UICR");
        log::info!(
            "Programming erased UICR HFXO settings for {} through NVMC",
            self.engine.target()
        );
        let mut mem = ProbeRsMemory(self.session.core(0)?);
        mem.0.reset_and_halt(Duration::from_secs(5))?;
        self.engine
            .write_hfxo_config(&mut mem, &self.clock, config, programmed)
            .map_err(Into::into)
    }

    fn process_zip_file(&mut self, mfw_zip: &Path) -> Result<(), ModemUpdateError> {
        self.segments.clear();
        self.firmware_update_digest = None;
        self.temp_dir = None;
        self.engine.set_pipelined(false);

        self.emit_status("Extracting firmware package");
        let temp_dir = TempDir::new()?;
        let file = File::open(mfw_zip)?;
        ZipArchive::new(file)?.extract(temp_dir.path())?;

        let digest_id = self.read_key_digest_string()?;

        let legacy_loader_re = legacy_loader_regex();
        let segment_re = Regex::new(r"^firmware\.update\.image\.segments\.(\d+)\.hex$")
            .expect("static regex literal");
        let mut modem_firmware_loader: Option<LoaderCandidate> = None;
        let mut segments = BTreeMap::new();

        for entry in std::fs::read_dir(&temp_dir)? {
            let entry = entry?;
            let os_name = entry.file_name();
            let Some(file_name) = os_name.to_str() else {
                log::debug!("Skipping non-UTF-8 file name in package");
                continue;
            };
            log::debug!("Processing file: {}", file_name);

            let loader = parse_loader_candidate(
                file_name,
                &digest_id,
                temp_dir.path().join(file_name),
                &legacy_loader_re,
            );
            if let Some(candidate) = loader {
                if modem_firmware_loader
                    .as_ref()
                    .is_none_or(|current| candidate.is_preferred_to(current))
                {
                    modem_firmware_loader = Some(candidate);
                }
            } else if let Some(c) = segment_re.captures(file_name) {
                let segment: u32 = c[1].parse().expect("\\d+ in regex");
                log::info!("Inserting segment: {}:{}", segment, file_name);
                segments.insert(segment, temp_dir.path().join(file_name));
            }
        }

        let modem_firmware_loader =
            modem_firmware_loader.ok_or(ModemUpdateError::LoaderNotFound)?;
        let LoaderVersion(major, minor, patch) = modem_firmware_loader.version;
        log::info!(
            "modem_firmware_loader version: {}.{}.{}",
            major,
            minor,
            patch
        );
        self.engine
            .set_pipelined(modem_firmware_loader.version > LoaderVersion(1, 1, 2));
        if self.engine.is_pipelined() {
            log::info!("Using pipelined loader");
        }

        if segments.is_empty() {
            return Err(ModemUpdateError::NoSegmentsFound);
        }

        let digest_path = temp_dir.path().join("firmware.update.image.digest.txt");
        log::debug!("Opening {}", digest_path.display());
        let firmware_update_digest = read_firmware_digest(&digest_path)?;

        log::info!(
            "Programming modem firmware loader: {}",
            modem_firmware_loader.path.display()
        );

        self.emit_status("Programming modem loader");
        flashing::download_file(
            self.session,
            &modem_firmware_loader.path,
            flashing::HexLoader,
        )?;

        self.emit_status("Starting modem loader");
        let mut mem = ProbeRsMemory(self.session.core(0)?);
        self.engine.start_loader(&mut mem, &self.clock)?;
        drop(mem);

        log::info!("modem_firmware_loader started!");
        // Keep the extracted package alive: `self.segments` and the digest
        // file paths point into it, and program_segments / verify_loaded
        // re-read them from disk.
        self.segments = segments;
        self.firmware_update_digest = Some(firmware_update_digest);
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
        let chunks: Vec<(usize, Vec<u8>)> = hex
            .segments()
            .chunks(Some(bufsz), Some(IPC_CHUNK_ALIGNMENT))?
            .into_iter()
            .collect();

        for (addr, _) in &chunks {
            if *addr % IPC_CHUNK_ALIGNMENT != 0 {
                return Err(ModemUpdateError::MisalignedAddress(*addr as u32));
            }
        }

        if self.engine.is_pipelined() {
            return self.program_chunks_pipelined(&chunks);
        }

        for (addr, data) in chunks {
            log::info!("Reading segment: {} with size {}", addr, data.len());

            let core = self.session.core(0)?;
            let mut mem = ProbeRsMemory(core);
            self.engine
                .write_chunk(&mut mem, &self.clock, addr as u32, &data, 0)?;
            drop(mem);

            self.increment_progress(data.len());
        }

        Ok(())
    }

    fn program_chunks_pipelined(
        &mut self,
        chunks: &[(usize, Vec<u8>)],
    ) -> Result<(), ModemUpdateError> {
        let Some((_, first_data)) = chunks.first() else {
            return Ok(());
        };

        {
            let mut mem = ProbeRsMemory(self.session.core(0)?);
            self.engine.stage_chunk(&mut mem, first_data, 0)?;
        }

        for (i, (addr, data)) in chunks.iter().enumerate() {
            let bank = (i % 2) as u8;
            log::info!("Committing segment: {} with size {}", addr, data.len());

            let mut mem = ProbeRsMemory(self.session.core(0)?);
            self.engine
                .commit_chunk(&mut mem, *addr as u32, data.len(), bank)?;

            if let Some((next_addr, next_data)) = chunks.get(i + 1) {
                let next_bank = ((i + 1) % 2) as u8;
                log::info!(
                    "Staging next segment: {} with size {} in bank {}",
                    next_addr,
                    next_data.len(),
                    next_bank
                );
                self.engine.stage_chunk(&mut mem, next_data, next_bank)?;
            }

            self.engine.wait_and_ack(&mut mem, &self.clock)?;
            drop(mem);

            self.increment_progress(data.len());
            log::info!("Wrote chunk: {}:{} for bank {}", i, addr, bank);
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
        ranges_to_verify.sort_unstable_by_key(|&(address, _)| address);

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

fn legacy_loader_regex() -> Regex {
    Regex::new(r"^([0-9A-Fa-f]{7})\.ipc_dfu\.signed_(\d+)\.(\d+)\.(\d+)\.ihex$")
        .expect("static regex literal")
}

fn parse_loader_candidate(
    file_name: &str,
    digest_id: &str,
    path: PathBuf,
    legacy_re: &Regex,
) -> Option<LoaderCandidate> {
    if let Some(captures) = legacy_re.captures(file_name) {
        if !captures[1].eq_ignore_ascii_case(digest_id) {
            return None;
        }
        return Some(LoaderCandidate {
            path,
            version: loader_version(&captures, 2),
        });
    }

    None
}

fn loader_version(captures: &regex::Captures<'_>, first: usize) -> LoaderVersion {
    LoaderVersion(
        captures[first].parse().expect("\\d+ in regex"),
        captures[first + 1].parse().expect("\\d+ in regex"),
        captures[first + 2].parse().expect("\\d+ in regex"),
    )
}

fn read_firmware_digest(path: &Path) -> Result<[u8; 32], ModemUpdateError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ModemUpdateError::NoDigestFound);
        }
        Err(error) => return Err(ModemUpdateError::Io(error)),
    };
    let digest_re =
        Regex::new(r"^SHA256 of all ranges in ascending address order:\s*([0-9A-Fa-f]{64})\s*$")
            .expect("static regex literal");

    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if let Some(captures) = digest_re.captures(&line) {
            let digest_hex = &captures[1];
            log::info!("Firmware digest: {}", digest_hex);
            return parse_sha256_hex(digest_hex).ok_or(ModemUpdateError::NoDigestFound);
        }
    }

    Err(ModemUpdateError::NoDigestFound)
}

fn parse_sha256_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(s.as_bytes()[i * 2])?;
        let lo = hex_nibble(s.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(file_name: &str, digest: &str) -> Option<LoaderCandidate> {
        parse_loader_candidate(
            file_name,
            digest,
            PathBuf::from(file_name),
            &legacy_loader_regex(),
        )
    }

    #[test]
    fn legacy_loader_must_match_digest_and_full_file_name() {
        let loader = candidate("ABC1234.ipc_dfu.signed_1.2.3.ihex", "ABC1234").unwrap();
        assert_eq!(loader.version, LoaderVersion(1, 2, 3));

        assert!(candidate("ABC1234.ipc_dfu.signed_1.2.3.ihex", "7654321").is_none());
        assert!(candidate("ABC1234.ipc_dfu.signed_1.2.3.ihex.bak", "ABC1234").is_none());
    }

    #[test]
    fn family_loader_without_digest_is_rejected() {
        assert!(candidate("ipc-dfu_nrf91x1_2.0.0.ihex", "ABC1234").is_none());
    }

    #[test]
    fn sha256_parser_rejects_invalid_input() {
        assert_eq!(
            parse_sha256_hex("000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F"),
            Some([
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
                23, 24, 25, 26, 27, 28, 29, 30, 31,
            ])
        );
        assert!(parse_sha256_hex("00").is_none());
        assert!(
            parse_sha256_hex("Z00102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F")
                .is_none()
        );
    }

    #[test]
    fn missing_digest_file_is_rejected_during_prepare() {
        let temp_dir = TempDir::new().unwrap();
        assert!(matches!(
            read_firmware_digest(&temp_dir.path().join("missing.txt")),
            Err(ModemUpdateError::NoDigestFound)
        ));
    }
}
