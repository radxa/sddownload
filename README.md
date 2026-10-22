# sddownload

Native Rust implementation of the SemiDrive PyDownload CLI. Easyboot, USB
fastboot, checksums, image handling, and ADB authentication run in process.
The program does not launch the bundled Python, easyboot, fastboot, or adb
executables.

J-Link support is intentionally omitted. PACs containing ELF FDA loaders can
be extracted and inspected, but cannot be downloaded by this program. They
are rejected before USB discovery or ADB reboot. No SEGGER SDK is needed.

## Build with Nix

From this directory:

```sh
nix build "path:$PWD"
./result/bin/sddownload --help
```

The explicit `path:` form also works before the files in this new repository
have been added to Git. Once tracked, ordinary `nix build` works as well.
`flake.lock` pins nixpkgs and `Cargo.lock` pins Rust dependencies.

Development commands:

```sh
nix develop "path:$PWD"
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The development shell keeps Cargo's target cache outside the source tree,
under `$XDG_CACHE_HOME/sddownload/target` or `~/.cache/sddownload/target`, unless
`CARGO_TARGET_DIR` is already set. This avoids copying build outputs when
evaluating a path flake. Package builds also filter out `target/` explicitly.

Without Nix, install Rust 1.87 or newer, pkg-config, and libusb development
files, then run `cargo build --release`. The Nix package runs the workspace
tests as part of its build. Validation is performed on x86-64 Linux; the
flake exposes Linux and Darwin package outputs for both x86-64 and AArch64.

## CLI compatibility

The existing long options retain their spelling, including underscores:

```text
--ospi1 FILE     --ospi2 FILE     --emmc1 FILE
--sd FILE        --ssd FILE       --pac FILE       --nvm FILE
--sn SERIAL      --b_part         --skip_verify
--auto           --reboot         --debug
--unpac_path DIRECTORY
--flash 'PARTITION|FILE'          (repeatable)
--erase 'PARTITION'               (repeatable)
--read 'PARTITION|FILE'            (repeatable)
-h / --help
```

Storage-specific PAC options use version 0. `--pac` uses version 1, the
all-in-one format. The PAC library itself understands both versions and
nested containers. Nonzero read/erase size suffixes are rejected for device
operations because they belonged to the removed J-Link backend. Extraction
does not invoke a device backend.

Additional options:

```text
--config APP_JSON       Explicit application configuration file
--adb-key PRIVATE_KEY   Explicit ADB RSA private-key file
--version
```

No application configuration file is implicitly loaded from the working
directory, executable directory, or Python distribution. Without `--config`,
the built-in values match `bin/configure/app.json`: 60-second device
detection, 20-second stage/DA operations, a 400-second default operation
timeout, and automatic erasure of empty `exception_log`/`misc` partitions.
Explicit JSON values override these defaults. A timeout of zero disables
that timeout. Legacy `settings`/`log` fields are accepted; `--debug` controls
Rust's retained extraction data and download-plan diagnostics.

Example configuration:

```json
{
  "timeout": {
    "detect_dev": 60,
    "get_stage": 20,
    "default": 400
  },
  "auto_erase": {
    "emmc": ["exception_log", "misc"]
  }
}
```

Extract without accessing USB:

```sh
sddownload --ospi1 firmware.pac --unpac_path ./images
```

Download through ROM/FDA and fastboot:

```sh
sddownload --config /path/to/app.json --ospi1 ospi.pac --emmc1 emmc.pac
sddownload --auto --flash 'emmc1$$spl|spl.img' --reboot
sddownload --ospi1 ospi.pac --read 'emmc1$$ssystem|ssystem.bin'
```

Use single quotes to preserve literal dollar signs. The old backslash-dollar
partition spelling is also normalized. No shell interprets partition names.
Flash, erase, reboot, and read operations interact with the connected device.

ADB uses a direct authenticated USB connection through `libadb`. Its default
key is `~/.android/adbkey`; a new private key is created with mode 0600 when
needed. The usual device authorization dialog may appear. A running ADB
server or another downloader can own the interface and prevent direct access.
USB operations also require the OS permissions described by the original
SDK's udev rules. This program does not modify system udev configuration.

## Libraries and protocol

* `sdimage`: streaming PAC parsing/CRC validation and extraction, a seekable
  expanded Android sparse-image reader, ELF32/ELF64 loader inspection.
  Uses `crc`, `md-5`, and `object`.
* `sdeasyboot`: raw ROM easyboot, extended fastboot, USB discovery, and native
  ADB reboot. Uses `rusb`, `fastboot-protocol`, and `libadb`. `BulkTransport`
  allows testing protocol code against simulated peers.
* `sddownload`: CLI, explicit config handling, PAC download planning, bootstrap
  ordering, A/B expansion, and operation dispatch. Its library can be reused
  by another frontend.

ADB authentication and connection handling use upstream `libadb` from crates.io,
without local patches. ADB is only used by `--auto` to request a bootloader reboot;
ROM and fastboot downloads do not use it. Upstream 0.6.0 rejects some legacy
NUL-terminated CNXN banners. If ADB auto-reboot fails, the CLI reports the error
and waits for download mode, which can be entered manually.

The USB mode comes from the raw configuration descriptor's `iConfiguration`
index: 0 is ROM, 4 is fastboot. Interfaces match class/subclass/protocol
`FF/42/03`; ADB uses protocol 01. A serial number selects among multiple
matching devices.

Easyboot sends raw FDA bytes in transfers no larger than 16 KiB. Fastboot
commands are unpadded ASCII with a 64-byte limit. SemiDrive's `MD5`, `read`,
and `flash-seq` extensions are layered over the existing fastboot codec.
Intermediate `flash-seq` commands do not wait for a response; only the final
segment does. Sparse interior DONT_CARE regions advance the logical offset;
a trailing hole is sent as zero bytes. Large-image transfers stay bounded
by the device's advertised download size.

PAC CRC uses CRC-32/BZIP2, excluding the checksum word at offset 0x3fc.
Stored zero/0xffffffff values mean unset. `--skip_verify` bypasses PAC CRC
and flash MD5 checks; readback MD5 is still required before publishing output.
Nested PAC checksums are checked as well. Bounds, unsafe partition paths,
invalid sparse geometry, and malformed protocol replies produce errors.
Extraction preserves unrelated files and refuses to replace differing files;
identical existing payloads may be reused. This is stricter than the legacy
overwriting behavior, especially when extracting several PACs into one folder.

Inspect PAC metadata through the library:

```sh
cargo run --example inspect_pac -- firmware.pac
```

## Verification boundary

Tests cover command-line handling, explicit configuration, nested/all-in-one
PACs, CRC mismatch/bypass, A/B and loader ordering, output preservation,
ELF32/ELF64 parsing, sparse expansion, raw ROM transfers, fastboot command
framing, segmented transfers and MD5-verified readback. Native ADB
authentication is exercised against a simulated USB peer with a temporary
RSA key. Tests do not access real USB devices.

Synthetic tests and successful Nix builds do not establish hardware
equivalence with every SemiDrive firmware version. No live flash, erase,
or reboot has been performed while implementing this Rust program.
