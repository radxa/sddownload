# sddownload

Native Rust implementation of the SemiDrive PyDownload CLI. Easyboot, USB
fastboot, checksums, image handling, and ADB authentication run in process.
The program does not launch the bundled Python, easyboot, fastboot, or adb
executables.

J-Link support is intentionally omitted. PACs can be extracted and inspected,
but downloads whose selected FDA loader is ELF are rejected. Use
`--verify-first` to detect unsupported loaders before USB
discovery or ADB reboot. No SEGGER SDK is needed.

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

Long options use hyphens between words:

```text
--ospi1 FILE     --ospi2 FILE     --emmc1 FILE
--sd FILE        --ssd FILE       --pac FILE       --nvm FILE
--sn SERIAL      --b-part         --skip-verify    --verify-first
--auto           --reboot         --debug
--unpac-path DIRECTORY
--flash PARTITION PATH           (repeatable; - reads stdin)
--erase 'PARTITION'               (repeatable)
--read PARTITION PATH            (repeatable; - writes stdout)
-h / --help
```

Storage-specific PAC options use version 0. `--pac` uses version 1, the
all-in-one format. The PAC library itself understands both versions and
nested containers. `--read` reads the whole named partition. Nonzero erase size
suffixes are rejected for device operations because they belonged to the removed J-Link backend. Extraction
does not invoke a device backend.

Additional options:

```text
--fda PATH              Raw ROM FDA image; - reads stdin; overrides PAC FDA
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
Rust's diagnostics and retention of temporary cached images.

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
sddownload --ospi1 firmware.pac --unpac-path ./images
```

Download through ROM/FDA and fastboot:

```sh
sddownload --config /path/to/app.json --ospi1 ospi.pac --emmc1 emmc.pac
sddownload --auto --flash 'emmc1$$spl' spl.img --reboot
sddownload --ospi1 ospi.pac --read 'emmc1$$ssystem' ssystem.bin
```

Use single quotes to preserve literal dollar signs. The old backslash-dollar
partition spelling is also normalized. No shell interprets partition names.
Flash, erase, reboot, and read operations interact with the connected device.

Supply a raw FDA to enter download mode without a PAC:

```sh
sddownload --fda fda.bin
cat fda.bin | sddownload --fda -
```

With no PAC or other operations, the command transfers the FDA if the device is
in ROM mode, waits for fastboot download mode, and exits successfully. A device
already in download mode does not need another FDA transfer. To continue with
PAC images or explicit flash operations, supply them in the same invocation:

```sh
sddownload --fda fda.bin --emmc1 firmware.pac
sddownload --fda fda.bin --flash 'emmc1$$kernel' kernel.img
```

`--fda` overrides every primary ROM FDA in the supplied PACs, including nested
and later copies; other download agents retain their usual bootstrap order.
Only one FDA, PAC, or explicit flash input may use stdin. The raw FDA is staged in a temporary
file to determine its size, so stdin must reach EOF before transfer starts.
Empty and ELF FDA images are rejected before device access. `--verify-first`
still validates all PAC CRCs before sending the FDA. `--fda` conflicts with the
extraction-only `--unpac-path` option.

`--flash` and `--read` each require two separate arguments, `PARTITION PATH`,
and may be repeated for different partitions. Quote partition names containing
dollar signs and paths containing spaces. The former `PARTITION|PATH` syntax
is no longer accepted.

Use `-` as the flash source for stdin, or as the readback destination for stdout:

```sh
zstd -dc kernel.img.zst | sddownload --fda fda.bin --flash 'emmc1$$kernel' -
sddownload --fda fda.bin --read 'emmc1$$kernel' - > kernel.img
sddownload --auto --flash 'emmc1$$boot' boot.img --flash 'emmc1$$system' system.img
```

A flash image from stdin is staged to disk before device access to determine
its length; stdin must reach EOF before flashing begins. Only one input across
FDA, PAC, and `--flash` can use stdin. Only one `--read` destination can use
stdout. Readback to stdout is staged to disk and published only after its MD5
and final device response are verified. Status and progress messages go to
stderr when reading to stdout, keeping redirected output binary-clean. A failed
verification publishes no readback bytes; an output write failure returns an
error. File readbacks continue to use atomic replacement after verification.

## Streaming PAC downloads

Downloads consume PAC input in one forward pass, reading payloads directly
instead of extracting every image first. Every PAC input option accepts `-`
for stdin; only one input may use stdin:

```sh
curl -fL https://example.com/global.pac | sddownload --emmc1 -
zstd -dc global.pac.zst | sddownload --emmc1 -
```

Each flash operation reports a progress bar on stderr with flashed/total image
size, recent USB transfer speed, and an estimated remaining time in `mm:ss`:

```text
Flash emmc1$rootfs [========        ]  50% 256.0 MiB/512.0 MiB 32.0 MiB/s ETA 00:08
```

The bar updates in place on a terminal; redirected stderr receives periodic
lines. `--:--` means there is not enough progress to estimate the time yet.
Sparse image sizes include skipped holes, while speed counts only transferred
payload bytes. The estimate follows recent image progress and excludes unknown
device-side write/verification delays. At 100%, the status remains `waiting for
device` until the final response succeeds; failures are marked `failed`. A/B
slots and explicit `--flash` images each get their own progress bar.

On Linux, Darwin, and Windows, normal mode uses a background input worker while
the calling thread performs USB operations serially. The input worker parses
headers, calculates CRCs, and continues reading/caching during FDA transfer,
device initialization, and flashes. USB can read a growing cache file before
the input worker finishes writing it: a `Caching ...` message does not mean
that the entire file must be cached before flashing starts.

When USB is ready, single-use payloads use a bounded in-memory pipe without a
temporary file. At a payload boundary, when USB is busy or the payload must
wait for a loader, input prefetch uses temporary cache files. The work queue and pipe buffers apply
backpressure. With `--b-part`, an A/B payload is cached while slot A is flashed,
then replayed for slot B; an already cached payload reuses its existing file.

Normal mode commits to the first eligible FDA/download agent of each kind,
without waiting for hypothetical replacements in unread nested headers. Known
earlier bootstrap stages are still respected. A later byte-identical loader is
ignored. A different replacement for an already selected loader, or a late
loader requiring an earlier bootstrap stage, fails with a recommendation to
use `--verify-first`. Later forward bootstrap stages (for example, an eMMC DA
in a second PAC after an OSPI DA) can still run without replacing the FDA.

There is no configurable cache limit; temporary storage must have enough free
space for prefetched, deferred, and reused data. Payload caches are removed
after use, including on I/O/protocol failure. Selected loader files are kept
until completion to compare late copies byte for byte. `--debug` retains cache
files and the working directory. The temporary directory follows the platform
default (`TMPDIR` on Unix). Input cancellation also interrupts blocked pipe
reads when USB fails. Windows uses binary input handles and cancels synchronous
I/O on the dedicated input thread, retrying cancellation until that thread
exits so a read starting just after cancellation cannot strand shutdown.
In-flight USB calls retain their configured timeouts. Other platforms retain
sequential scheduling.

By default, CRCs are calculated as the input is consumed and checked when each
PAC container ends. A CRC failure or late-loader conflict can therefore be
reported after device writes have occurred. Failure stops further scheduling,
returns a nonzero exit status, and prevents the final requested reboot.
Fastboot transfer MD5 verification remains enabled.

Use `--verify-first` to finish PAC CRC validation before any ADB/USB device
operations. This stages the required payloads from all inputs in temporary
files and selects the last FDA/download agent of each kind, then downloads
after validation. It retains the original global last-loader policy and does
not use early bootstrap:

```sh
zstd -dc global.pac.zst | sddownload --emmc1 - --verify-first
```

`--verify-first` conflicts with `--skip-verify`, which disables PAC CRC and
flash MD5 checks. Unset PAC CRC fields remain optional in both policies.
`--unpac-path` still explicitly extracts files; stdin is first staged to a
temporary PAC for that mode. Streaming traversal orders entries by physical
offset and rejects overlapping entries.

ADB uses a direct authenticated USB connection through `libadb`. Its default
key is `~/.android/adbkey`; a new private key is created with mode 0600 when
needed. The usual device authorization dialog may appear. A running ADB
server or another downloader can own the interface and prevent direct access.
USB operations also require the OS permissions described by the original
SDK's udev rules. This program does not modify system udev configuration.

## Libraries and protocol

* `sdimage`: PAC parsing/CRC validation and extraction, forward-only PAC and
  sparse-image readers, a seekable sparse-image reader, ELF32/ELF64 inspection.
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
Stored zero/0xffffffff values mean unset. `--skip-verify` bypasses PAC CRC
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

Inspect and CRC-check a forward-only stream without accessing USB:

```sh
zstd -dc firmware.pac.zst | cargo run --release --example stream_pac -- -
```

## Verification boundary

Tests cover command-line handling, explicit configuration, nested/all-in-one
PACs, CRC mismatch/bypass, A/B and loader ordering, output preservation,
ELF32/ELF64 parsing, sparse expansion, raw ROM transfers, fastboot command
framing, segmented transfers and MD5-verified readback. Native ADB
authentication is exercised against a simulated USB peer with a temporary
RSA key. Tests do not access real USB devices.

Concurrency tests use controlled input and simulated USB targets to verify
early FDA startup, input progress during boot, reads from growing caches,
duplicate/conflicting late loaders, cancellation with an open input pipe, and
cleanup after errors or a target panic.

Synthetic tests and successful Nix builds do not establish hardware
equivalence with every SemiDrive firmware version. No live flash, erase,
or reboot has been performed while implementing this Rust program.
