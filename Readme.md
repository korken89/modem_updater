# nRF91 Modem Updater using `probe-rs`

## Summary

This is a tool to update the nRF91 modem firmware using the `probe-rs` crate. It provides both a CLI and library interface. Used in production on the [nRF9160 Feather](https://www.circuitdojo.com/products/nrf9160-feather) and [nRF9151 Feather](https://www.circuitdojo.com/products/nrf9151-feather).

Validated working on:

- nRF9160
- nRF9151
- nRF9161

## Getting Started

### 1. Install the Rust toolchain

Install Rust using [`rustup`](https://rustup.rs/).

On macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Windows download and run the installer from [rustup.rs](https://rustup.rs/), then open a new PowerShell or Command Prompt window.

After installation, confirm the toolchain is available:

```bash
rustc --version
cargo --version
```

### 2. Install with `cargo`

```bash
cargo install --git https://github.com/circuitdojo/modem_updater.git
```

This places the `updater` binary in `~/.cargo/bin` (or the equivalent on Windows).

### 3. Install probe dependencies

Sometimes you may need to install dependencies. If your compilation fails here are some suggestions:

- **Linux:** ensure `libusb-1.0` and `libudev-dev` are present (`sudo apt install libusb-1.0-0 libudev-dev`).
- **macOS:** install [Homebrew](https://brew.sh/) and run `brew install libusb` if it is not already available.
- **Windows:** if necessary, use [Zadig](https://zadig.akeo.ie/) to install a WinUSB driver for your debug probe.

## CLI Usage

The CLI exposes two subcommands, `verify` and `program`:

```bash
# Verify the firmware on the connected device matches the package
updater --target nrf9151 verify <path_to_firmware_zip>

# Program and verify in one go
updater --target nrf9151 program <path_to_firmware_zip>
```

`--target` is required and selects the chip variant (`nrf9151` or `nrf9160`).
If debug access is blocked, `updater` will attempt to restore access
non-interactively before continuing.

### Selecting a probe

With a single probe connected, no extra flags are needed. With multiple
probes, narrow the selection with any of:

- `--vid 0x1366` / `--pid 0x1059` - match by USB vendor/product ID
- `--serial <serial-number>` - match a specific probe
- `--all-probes` - run the command against every matching probe (in
  parallel; see below)

### Mass programming with `--all-probes`

`updater --target nrf9151 --all-probes program <zip>` spawns one worker
per matching probe and programs them concurrently. Each probe gets its
own progress bar whose message tracks the current phase
(`Preparing device`, `Programming device`, `Verifying`,
`Verification success`/`failed`). The process reports any per-probe
failures and exits non-zero if any failed.

### Probe speed

`--speed <kHz>` sets the SWD clock (default 12000). Lower this if your
probe rejects the requested speed (e.g. some J-Links don't support
arbitrary frequencies).

## Developement

### 1. Install the Rust toolchain

Install Rust using [`rustup`](https://rustup.rs/).

On macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Windows download and run the installer from [rustup.rs](https://rustup.rs/), then open a new PowerShell or Command Prompt window.

After installation, confirm the toolchain is available:

```bash
rustc --version
cargo --version
```

### 2. Clone this repository

```bash
git clone https://github.com/circuitdojo/modem_updater.git
cd modem_updater
```

### 3. Build the project

Build a release binary locally:

```bash
cargo build --release
```

The resulting executable are located in `target/release/`:
- `updater` - Main firmware update tool

### 4. Target profiles and recovery

The library and CLI support both `nrf9151` and `nrf9160` target profiles.

- `updater` requires `--target nrf9151 | nrf9160` to select the profile.
- If debug access is blocked, `updater` first tries to restore access
  non-interactively.
- If the device is still locked, the nRF91 CTRL-AP erase-and-reset flow
  runs automatically (probe-rs target sequence with `allow_erase_all`).

This split exists because the modem DFU bring-up sequence is not
identical across the chips. In particular, UICR programming and IPC
receive masks differ between the profiles.

## Library usage

The crate is split into two layers:

- `modem_updater::protocol` - `no_std`-compatible IPC DFU state machine.
  Operates on `ModemMemory` and `Clock` traits; no probe-rs or
  filesystem dependencies. Use this to drive modem updates from
  external flash on embedded targets. For `no_std` consumers, disable
  default features:

  ```toml
  modem_updater = { version = "0.1", default-features = false }
  ```

- `modem_updater::ModemUpdater` (host wrapper, requires the `std`
  feature) glues probe-rs to `ModemMemory`, parses Nordic `.zip`
  packages, and exposes both one-shot helpers (`verify`,
  `program_and_verify`) and a phased API (`prepare`,
  `program_segments`, `verify_loaded`). Use the phased API when
  orchestrating across multiple probes.

Cargo features:

- `default = ["cli"]`
- `std` - enables the host wrapper (probe-rs, zip, ihex, tempfile)
- `cli` - implies `std`, plus the CLI dependencies (clap, indicatif)

## Acknowledgements

This project is based on the work of [**@maxd-nordic**](https://github.com/maxd-nordic) in the [pyOCD](https://github.com/pyocd/pyOCD/blob/5166025ae5da5e093d6cfe2b26cae5e1334476e4/pyocd/target/family/target_nRF91.py#L629) project.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0).
  Apache 2.0 applies with a trademark modification (Section 6 is
  replaced; see the preamble in [LICENSE-APACHE](LICENSE-APACHE)).
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.
