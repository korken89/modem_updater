// Copyright 2025 Jared Wolff
//
// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// Apache-2.0 applies with the trademark modification described in
// LICENSE-APACHE.

use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use modem_updater::{ModemUpdater, TargetProfile};
use probe_rs::{
    probe::{list::Lister, DebugProbeInfo, DebugProbeSelector, Probe},
    Permissions, Session,
};

/// Update nRF91 modem firmware over a debug probe.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Target nRF91 chip variant (e.g. `nrf9151` or `nrf9160`).
    #[arg(long, short = 't', value_parser = parse_target)]
    target: TargetProfile,

    /// USB vendor ID of the debug probe (e.g. 0x2e8a or 11914).
    #[arg(long, value_parser = parse_u16)]
    vid: Option<u16>,

    /// USB product ID of the debug probe (e.g. 0x000c or 12).
    #[arg(long, value_parser = parse_u16)]
    pid: Option<u16>,

    /// Serial number of the debug probe.
    #[arg(long)]
    serial: Option<String>,

    /// SWD/JTAG clock speed in kHz. Defaults to 12000; lower this if your
    /// probe rejects the requested speed (e.g. some J-Links).
    #[arg(long, default_value_t = 12_000)]
    speed: u32,

    /// Run the command against every connected probe (filtered by
    /// `--vid`/`--pid` if given), rather than requiring exactly one match.
    /// Cannot be combined with `--serial`.
    #[arg(long = "all-probes", conflicts_with = "serial")]
    all_probes: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Verify firmware on the device against the given package without programming.
    Verify {
        /// Path to the modem firmware .zip package.
        path: PathBuf,
    },
    /// Program and verify firmware from the given package.
    Program {
        /// Path to the modem firmware .zip package.
        path: PathBuf,
    },
}

fn parse_u16(s: &str) -> Result<u16, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).map_err(|e| format!("invalid hex u16: {e}"))
    } else {
        s.parse::<u16>().map_err(|e| format!("invalid u16: {e}"))
    }
}

fn parse_target(s: &str) -> Result<TargetProfile, String> {
    s.parse::<TargetProfile>().map_err(|e| e.to_string())
}

/// Selects which debug probes the command should run against.
///
/// Without `--all-probes`, exactly one probe must match the
/// `--vid`/`--pid`/`--serial` filters. With `--all-probes`, every matching
/// probe is returned (filter still applies, but multiple matches are OK).
fn select_probes(lister: &Lister, cli: &Cli) -> Result<Vec<DebugProbeSelector>, String> {
    let probes = lister.list_all();

    let matches: Vec<&DebugProbeInfo> = probes
        .iter()
        .filter(|p| cli.vid.is_none_or(|v| p.vendor_id == v))
        .filter(|p| cli.pid.is_none_or(|v| p.product_id == v))
        .filter(|p| {
            cli.serial
                .as_deref()
                .is_none_or(|s| p.serial_number.as_deref() == Some(s))
        })
        .collect();

    if matches.is_empty() {
        if probes.is_empty() {
            return Err(
                "No debug probe detected. Connect a programmer via USB and try again.".to_string(),
            );
        }
        let mut msg =
            String::from("No debug probe matched the supplied filter. Connected probes:\n");
        for p in &probes {
            msg.push_str(&format!("  - {}\n", p));
        }
        return Err(msg);
    }

    if cli.all_probes || matches.len() == 1 {
        return Ok(matches.iter().map(|p| selector_for(p)).collect());
    }

    let mut msg = String::from(
        "Multiple debug probes connected. Specify --vid/--pid/--serial to disambiguate, or pass --all-probes to run on all of them:\n",
    );
    for p in &matches {
        msg.push_str(&format!("  - {}\n", p));
    }
    Err(msg)
}

fn selector_for(info: &DebugProbeInfo) -> DebugProbeSelector {
    DebugProbeSelector {
        vendor_id: info.vendor_id,
        product_id: info.product_id,
        interface: info.interface,
        serial_number: info.serial_number.clone(),
    }
}

fn probe_label(selector: &DebugProbeSelector) -> String {
    format!(
        "{:04x}:{:04x}:{}",
        selector.vendor_id,
        selector.product_id,
        selector.serial_number.as_deref().unwrap_or("?")
    )
}

fn open_probe(
    lister: &Lister,
    selector: &DebugProbeSelector,
    speed_khz: u32,
) -> Result<Probe, String> {
    let start = Instant::now();
    let timeout = Duration::from_secs(2);

    // Suppress panic output from probe-rs internals (e.g. Glasgow driver)
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lister.open(selector.clone())
        }));

        match result {
            Ok(Ok(mut probe)) => {
                std::panic::set_hook(default_hook);
                if let Err(err) = probe.set_speed(speed_khz) {
                    log::warn!(
                        "Unable to set probe speed to {} kHz: {}. Using default.",
                        speed_khz,
                        err
                    );
                }
                return Ok(probe);
            }
            Ok(Err(_)) | Err(_) => {
                if start.elapsed() > timeout {
                    std::panic::set_hook(default_hook);

                    let probes = lister.list(Some(selector));
                    let msg = if probes.is_empty() {
                        "No debug probe detected. Please check that the programmer is connected via USB and powered on.".to_string()
                    } else {
                        "Debug probe found but unable to initialize. Please check that the target board is connected to the programmer.".to_string()
                    };
                    return Err(msg);
                }

                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Attaches to the chip with `allow_erase_all`, which lets probe-rs's nRF91
/// target sequence handle APPROTECT unlock automatically via CTRL-AP ERASEALL
/// + soft reset when needed.
fn attach_session(
    lister: &Lister,
    selector: &DebugProbeSelector,
    chip: TargetProfile,
    speed_khz: u32,
) -> Result<Session, String> {
    let probe = open_probe(lister, selector, speed_khz)?;

    probe
        .attach(
            chip.probe_rs_target_name(),
            Permissions::new().allow_erase_all(),
        )
        .map_err(|err| format!("Unable to attach to {}: {}", chip, err))
}

fn main() {
    env_logger::init();

    let cli = Cli::parse();

    let lister = Lister::new();
    let selectors = select_probes(&lister, &cli).unwrap_or_else(|err| {
        eprintln!("{}", err);
        std::process::exit(1);
    });

    drop(lister);

    if !run(&selectors, &cli) {
        std::process::exit(2);
    }
}

/// Spawn one worker per probe via [`thread::scope`]; each worker owns its
/// own probe-rs `Session` and a single `ProgressBar` whose message changes
/// as it moves through prepare → program → verify. In a TTY, the bar is the
/// only output for the happy path. In a non-TTY (CI, piped output) the bar
/// is silent — set `RUST_LOG=info` to surface progress in logs.
fn run(selectors: &[DebugProbeSelector], cli: &Cli) -> bool {
    let target = cli.target;
    let speed = cli.speed;
    let path: &Path = match &cli.command {
        Command::Verify { path } | Command::Program { path } => path,
    };
    let do_program = matches!(&cli.command, Command::Program { .. });
    let mp = MultiProgress::new();

    let (tx, rx) = mpsc::channel::<(String, bool)>();
    let mut failures: Vec<String> = Vec::new();

    thread::scope(|s| {
        for selector in selectors {
            let label = probe_label(selector);
            let tx = tx.clone();
            let mp = mp.clone();
            s.spawn(move || {
                // `Lister` holds `Box<dyn ProbeLister>`, which is !Sync, so it
                // can't be borrowed across threads. Constructing a fresh
                // Lister per worker is cheap — it just registers the built-in
                // probe drivers.
                let lister = Lister::new();
                let success = run_one_probe(&lister, selector, target, speed, path, do_program, &mp, &label);
                let _ = tx.send((label, success));
            });
        }
        drop(tx);

        for (label, ok) in rx.iter() {
            if !ok {
                failures.push(label);
            }
        }
    });

    if !failures.is_empty() {
        eprintln!(
            "\n{}/{} probe(s) failed: {}",
            failures.len(),
            selectors.len(),
            failures.join(", ")
        );
        return false;
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn run_one_probe(
    lister: &Lister,
    selector: &DebugProbeSelector,
    target: TargetProfile,
    speed: u32,
    path: &Path,
    do_program: bool,
    mp: &MultiProgress,
    label: &str,
) -> bool {
    let bar = mp.add(ProgressBar::new(0));
    bar.set_prefix(format!("[{}]", label));
    bar.set_style(idle_progress_style());
    bar.set_message("Preparing device");
    bar.enable_steady_tick(Duration::from_millis(100));

    let mut session = match attach_session(lister, selector, target, speed) {
        Ok(s) => s,
        Err(err) => {
            bar.finish_with_message(format!("attach error: {}", err));
            return false;
        }
    };

    let mut updater = ModemUpdater::new_with_target(&mut session, target);

    if let Err(err) = updater.prepare(path) {
        bar.finish_with_message(format!("prepare error: {}", err));
        return false;
    }

    if do_program {
        bar.set_style(programming_progress_style());
        bar.set_message("Programming device");

        let bar_for_cb = bar.clone();
        updater.set_progress_callback({
            let mut initialized = false;
            move |cur, tot| {
                if !initialized {
                    bar_for_cb.set_length(tot);
                    initialized = true;
                }
                bar_for_cb.set_position(cur.min(tot));
            }
        });

        if let Err(err) = updater.program_segments() {
            bar.finish_with_message(format!("program error: {}", err));
            return false;
        }
    }

    bar.set_style(idle_progress_style());
    bar.set_message("Verifying");

    let (msg, ok) = match updater.verify_loaded() {
        Ok(true) => ("Verification success".to_string(), true),
        Ok(false) => ("Verification failed".to_string(), false),
        Err(err) => (format!("Error: {}", err), false),
    };
    bar.finish_with_message(msg);
    ok
}

/// Spinner + message for phases without byte-level progress (prepare,
/// verify, final result).
fn idle_progress_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix} {spinner:.green} {msg}")
        .expect("static progress-bar template")
}

/// Bar + bytes for the segment-writing phase.
fn programming_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix} {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}",
    )
    .expect("static progress-bar template")
    .with_key(
        "bytes",
        |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = write!(w, "{}", HumanBytes(state.pos()));
        },
    )
    .with_key(
        "total_bytes",
        |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            if let Some(len) = state.len() {
                let _ = write!(w, "{}", HumanBytes(len));
            } else {
                let _ = w.write_str("0 B");
            }
        },
    )
    .progress_chars("=>-")
}

