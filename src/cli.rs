use crate::{config::Config, plan::Plan, size};
use anyhow::{ensure, Result};
use clap::Parser;
use sdeasyboot::{
    fastboot::FastbootClient,
    usb::{self, Mode, UsbLink},
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

mod listing_table;
mod partitions;
mod progress;
mod streaming;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "sddownload",
    version,
    infer_long_args = true,
    args_override_self = true,
    about = "SemiDrive downloader"
)]
pub struct Cli {
    /// List PAC metadata or query fastboot partitions; --verify-first checks CRCs.
    #[arg(long, conflicts_with_all = ["flash", "read", "erase", "reboot", "unpac_path"])]
    pub list_partitions: bool,
    /// Raw ROM FDA image, overriding PAC FDA images; use - for stdin.
    #[arg(long, value_name = "PATH", conflicts_with = "unpac_path")]
    pub fda: Option<PathBuf>,
    /// PAC image for the first OSPI storage device; use - for stdin.
    #[arg(long)]
    pub ospi1: Option<PathBuf>,
    /// PAC image for the second OSPI storage device; use - for stdin.
    #[arg(long)]
    pub ospi2: Option<PathBuf>,
    /// PAC image for eMMC storage; use - for stdin.
    #[arg(long)]
    pub emmc1: Option<PathBuf>,
    /// PAC image for SD card storage; use - for stdin.
    #[arg(long)]
    pub sd: Option<PathBuf>,
    /// PAC image for SSD storage; use - for stdin.
    #[arg(long)]
    pub ssd: Option<PathBuf>,
    /// All-in-one PAC image (version 1); use - for stdin.
    #[arg(long)]
    pub pac: Option<PathBuf>,
    /// PAC image for NVM storage; use - for stdin.
    #[arg(long)]
    pub nvm: Option<PathBuf>,

    /// Select a USB device by serial number.
    #[arg(long, default_value = "")]
    pub sn: String,
    /// Flash both slots for images marked as A/B.
    #[arg(long)]
    pub b_part: bool,
    /// Skip PAC CRC and flash MD5 verification.
    #[arg(long)]
    pub skip_verify: bool,
    /// Validate PACs and select final loaders before device access; caches payloads.
    #[arg(long, conflicts_with = "skip_verify")]
    pub verify_first: bool,
    /// Try rebooting into download mode through ADB before connecting.
    #[arg(long = "auto")]
    pub automatic: bool,
    /// Reboot the device after successful operations.
    #[arg(long)]
    pub reboot: bool,
    /// Print diagnostics and retain temporary cached images.
    #[arg(long)]
    pub debug: bool,
    /// Extract PAC images to this directory without accessing USB.
    #[arg(long, value_name = "DIRECTORY")]
    pub unpac_path: Option<PathBuf>,

    /// Flash image or matching PAC resource; PATH may be - for stdin (repeatable).
    #[arg(long, num_args = 2, value_names = ["PARTITION", "PATH"], action = clap::ArgAction::Append)]
    pub flash: Vec<String>,
    /// Erase a partition (repeatable).
    #[arg(long, value_name = "PARTITION", action = clap::ArgAction::Append)]
    pub erase: Vec<String>,
    /// Read a partition into a file; PATH may be - for stdout (repeatable).
    #[arg(long, num_args = 2, value_names = ["PARTITION", "PATH"], action = clap::ArgAction::Append)]
    pub read: Vec<String>,

    /// Load timeout and auto-erase settings from a JSON file.
    #[arg(long, value_name = "APP_JSON")]
    pub config: Option<PathBuf>,
    /// Use this ADB private key instead of ~/.android/adbkey.
    #[arg(long, value_name = "PRIVATE_KEY")]
    pub adb_key: Option<PathBuf>,
}

#[derive(Default, Debug)]
struct Actions {
    flash: Vec<(String, PathBuf)>,
    pac_flash: Vec<PacFlash>,
    erase: Vec<(String, u64)>,
    read: Vec<(String, PathBuf)>,
}

#[derive(Debug)]
struct PacFlash {
    partition: String,
    path: PathBuf,
    storages: Vec<&'static str>,
}

impl PacFlash {
    fn matches(&self, storage: &str, entry: &sdimage::pac::Entry) -> bool {
        self.storages.contains(&storage)
            && self.partition == crate::plan::storage_partition(storage, &entry.resource_name)
    }
}

impl Actions {
    fn is_empty(&self) -> bool {
        self.flash.is_empty()
            && self.pac_flash.is_empty()
            && self.erase.is_empty()
            && self.read.is_empty()
    }
}

fn same_input_path(left: &Path, right: &Path) -> bool {
    left == right
        || (left != Path::new("-")
            && right != Path::new("-")
            && std::fs::canonicalize(left)
                .ok()
                .zip(std::fs::canonicalize(right).ok())
                .is_some_and(|(left, right)| left == right))
}

impl Cli {
    fn validate_stdin(&self) -> Result<()> {
        let pac_stdin = self
            .inputs()
            .iter()
            .filter(|(_, path)| *path == Path::new("-"))
            .count();
        let fda_stdin = usize::from(self.fda.as_deref() == Some(Path::new("-")));
        let flash_stdin = self
            .flash
            .chunks_exact(2)
            // References to a PAC stdin input share that stream instead of
            // declaring another raw-image stdin consumer.
            .filter(|pair| pair[1] == "-" && pac_stdin == 0)
            .count();
        ensure!(
            pac_stdin + fda_stdin + flash_stdin <= 1,
            "stdin can only be used for one input (FDA, PAC, or --flash)"
        );
        Ok(())
    }

    pub fn inputs(&self) -> Vec<(&'static str, &Path)> {
        [
            ("ospi1", &self.ospi1),
            ("ospi2", &self.ospi2),
            ("emmc1", &self.emmc1),
            ("sd", &self.sd),
            ("pac", &self.pac),
            ("ssd", &self.ssd),
            ("nvm", &self.nvm),
        ]
        .into_iter()
        .filter_map(|(name, path)| path.as_deref().map(|path| (name, path)))
        .collect()
    }

    fn actions(&self) -> Result<Actions> {
        ensure!(
            self.fda.is_some() || self.automatic || !self.inputs().is_empty(),
            "specify --fda, a PAC input, or --auto"
        );
        self.validate_stdin()?;

        if let Some(output) = &self.unpac_path {
            ensure!(
                !output.as_os_str().is_empty(),
                "--unpac-path requires a directory"
            );
        }

        let mut actions = Actions {
            read: partition_paths(&self.read, "--read")?,
            ..Actions::default()
        };
        for (partition, path) in partition_paths(&self.flash, "--flash")? {
            ensure!(
                path == Path::new("-") || path.is_file(),
                "flash image does not exist: {}",
                path.display()
            );
            let storages: Vec<_> = self
                .inputs()
                .into_iter()
                .filter(|(_, input)| same_input_path(input, &path))
                .map(|(storage, _)| storage)
                .collect();
            if storages.is_empty() {
                actions.flash.push((partition, path));
            } else {
                ensure!(
                    !actions
                        .pac_flash
                        .iter()
                        .any(|previous| previous.partition == partition
                            && previous
                                .storages
                                .iter()
                                .any(|storage| storages.contains(storage))),
                    "duplicate PAC --flash selection: {partition}"
                );
                actions.pac_flash.push(PacFlash {
                    partition,
                    path,
                    storages,
                });
            }
        }
        ensure!(
            actions
                .read
                .iter()
                .filter(|(_, path)| path == Path::new("-"))
                .count()
                <= 1,
            "stdout can only be used for one --read output"
        );

        for raw in &self.erase {
            let parts: Vec<_> = raw.split('|').collect();

            ensure!(
                (1..=2).contains(&parts.len()) && !parts[0].is_empty(),
                "--erase requires PARTITION[|SIZE]"
            );

            let count = if parts.len() == 2 {
                size::parse(parts[1])?
            } else {
                0
            };
            actions.erase.push((parts[0].replace("\\$", "$"), count));
        }

        Ok(actions)
    }
}

fn partition_paths(values: &[String], option: &str) -> Result<Vec<(String, PathBuf)>> {
    let pairs = values.chunks_exact(2);
    ensure!(
        pairs.remainder().is_empty(),
        "{option} requires PARTITION PATH"
    );
    pairs
        .map(|pair| {
            ensure!(
                !pair[0].is_empty() && !pair[1].is_empty(),
                "{option} requires nonempty PARTITION and PATH"
            );
            Ok((pair[0].replace("\\$", "$"), PathBuf::from(&pair[1])))
        })
        .collect()
}

fn with_fastboot<R>(
    options: &Cli,
    timeout: Duration,
    operation: impl FnOnce(&mut FastbootClient<UsbLink>) -> Result<R>,
) -> Result<R> {
    let info = usb::wait_for_mode(&options.sn, timeout, Mode::Fastboot)?;
    let mut client = FastbootClient::new(UsbLink::open(info, timeout)?);

    operation(&mut client)
}

pub fn execute(options: &Cli) -> Result<()> {
    if options.list_partitions {
        return partitions::run(options);
    }
    let actions = options.actions()?;
    let settings = Config::load(options.config.as_deref())?;

    if options.unpac_path.is_none() {
        ensure!(
            actions.erase.iter().all(|(_, size)| *size == 0),
            "sized erase arguments require J-Link, which is not supported"
        );
    }

    let temporary = tempfile::Builder::new().prefix("sddownload-").tempdir()?;
    let temporary = if options.debug {
        let path = temporary.keep();
        sdeasyboot::statusln!("Debug images retained in {}", path.display());
        Temporary::Kept(path)
    } else {
        Temporary::Owned(temporary)
    };

    if options.unpac_path.is_none() {
        streaming::download(options, &settings, &actions, temporary.path())?;
        if actions.read.iter().any(|(_, path)| path == Path::new("-")) {
            sdeasyboot::statusln!("Download successfully");
        } else {
            sdeasyboot::console::suspend(|| println!("Download successfully"));
        }
        return Ok(());
    }

    let mut extraction = options.clone();
    let stdin_inputs = extraction
        .inputs()
        .iter()
        .filter(|(_, path)| *path == Path::new("-"))
        .count();
    ensure!(
        stdin_inputs <= 1,
        "stdin can only be used for one PAC input"
    );
    for input in [
        &mut extraction.ospi1,
        &mut extraction.ospi2,
        &mut extraction.emmc1,
        &mut extraction.sd,
        &mut extraction.ssd,
        &mut extraction.pac,
        &mut extraction.nvm,
    ] {
        if input.as_deref() == Some(Path::new("-")) {
            let path = temporary.path().join("stdin.pac");
            std::io::copy(&mut std::io::stdin(), &mut std::fs::File::create(&path)?)?;
            *input = Some(path);
        }
    }
    let plan = Plan::prepare(&extraction, &settings, temporary.path())?;

    if options.debug {
        sdeasyboot::statusln!("Download plan: {plan:#?}");
    }

    sdeasyboot::console::suspend(|| println!("Unpac successfully"));

    Ok(())
}

enum Temporary {
    Owned(tempfile::TempDir),
    Kept(PathBuf),
}

impl Temporary {
    fn path(&self) -> &Path {
        match self {
            Self::Owned(dir) => dir.path(),
            Self::Kept(path) => path,
        }
    }
}

pub fn run_from(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> i32 {
    let options = match Cli::try_parse_from(args) {
        Ok(options) => options,
        Err(error) => {
            let success = matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );

            let _ = error.print();

            return if success { 0 } else { 1 };
        }
    };

    match execute(&options) {
        Ok(()) => 0,
        Err(error) => {
            sdeasyboot::statusln!("sddownload: {error:#}");
            1
        }
    }
}
