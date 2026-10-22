use crate::{
    config::Config,
    plan::{self, Plan},
    size,
};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use sdeasyboot::{
    fastboot::{DeviceFailure, FastbootClient},
    usb::{self, EasybootClient, Mode, UsbLink},
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

#[derive(Debug, Parser)]
#[command(
    name = "sddownload",
    version,
    infer_long_args = true,
    args_override_self = true,
    about = "SemiDrive downloader"
)]
pub struct Cli {
    /// PAC image for the first OSPI storage device.
    #[arg(long)]
    pub ospi1: Option<PathBuf>,
    /// PAC image for the second OSPI storage device.
    #[arg(long)]
    pub ospi2: Option<PathBuf>,
    /// PAC image for eMMC storage.
    #[arg(long)]
    pub emmc1: Option<PathBuf>,
    /// PAC image for SD card storage.
    #[arg(long)]
    pub sd: Option<PathBuf>,
    /// PAC image for SSD storage.
    #[arg(long)]
    pub ssd: Option<PathBuf>,
    /// All-in-one PAC image (version 1).
    #[arg(long)]
    pub pac: Option<PathBuf>,
    /// PAC image for NVM storage.
    #[arg(long)]
    pub nvm: Option<PathBuf>,

    /// Select a USB device by serial number.
    #[arg(long, default_value = "")]
    pub sn: String,
    /// Flash both slots for images marked as A/B.
    #[arg(long = "b_part")]
    pub b_part: bool,
    /// Skip PAC CRC and flash MD5 verification.
    #[arg(long = "skip_verify")]
    pub skip_verify: bool,
    /// Try rebooting into download mode through ADB before connecting.
    #[arg(long = "auto")]
    pub automatic: bool,
    /// Reboot the device after successful operations.
    #[arg(long)]
    pub reboot: bool,
    /// Print the download plan and keep temporary images.
    #[arg(long)]
    pub debug: bool,
    /// Extract PAC images to this directory without accessing USB.
    #[arg(long = "unpac_path", value_name = "DIRECTORY")]
    pub unpac_path: Option<PathBuf>,

    /// Flash an image to a partition (repeatable).
    #[arg(long, value_name = "PARTITION|PATH", action = clap::ArgAction::Append)]
    pub flash: Vec<String>,
    /// Erase a partition (repeatable).
    #[arg(long, value_name = "PARTITION", action = clap::ArgAction::Append)]
    pub erase: Vec<String>,
    /// Read a partition into a file (repeatable).
    #[arg(long, value_name = "PARTITION|PATH", action = clap::ArgAction::Append)]
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
    erase: Vec<(String, u64)>,
    read: Vec<(String, PathBuf, u64)>,
}

impl Actions {
    fn is_empty(&self) -> bool {
        self.flash.is_empty() && self.erase.is_empty() && self.read.is_empty()
    }
}

impl Cli {
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
            self.automatic || !self.inputs().is_empty(),
            "specify a PAC input or --auto"
        );

        if let Some(output) = &self.unpac_path {
            ensure!(
                !output.as_os_str().is_empty(),
                "--unpac_path requires a directory"
            );
        }

        let mut actions = Actions::default();

        for raw in &self.flash {
            let (partition, path) = raw
                .split_once('|')
                .context("--flash requires PARTITION|PATH")?;

            ensure!(!partition.is_empty() && !path.is_empty(), "invalid --flash");

            ensure!(
                Path::new(path).is_file(),
                "flash image does not exist: {path}"
            );

            actions
                .flash
                .push((partition.replace("\\$", "$"), path.into()));
        }

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

        for raw in &self.read {
            let parts: Vec<_> = raw.split('|').collect();

            ensure!(
                (2..=3).contains(&parts.len()) && !parts[0].is_empty() && !parts[1].is_empty(),
                "--read requires PARTITION|PATH[|SIZE]"
            );

            let count = if parts.len() == 3 {
                size::parse(parts[2])?
            } else {
                0
            };
            actions
                .read
                .push((parts[0].replace("\\$", "$"), parts[1].into(), count));
        }

        Ok(actions)
    }
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

fn bootstrap(options: &Cli, settings: &Config, plan: &Plan) -> Result<()> {
    if options.automatic {
        let key = options
            .adb_key
            .clone()
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".android/adbkey"))
            })
            .context("set --adb-key when HOME is unavailable")?;

        if let Err(error) =
            sdeasyboot::adb::reboot_bootloader(&options.sn, &key, settings.timeout("detect_dev"))
        {
            eprintln!("ADB auto-reboot unavailable: {error:#}; detecting download mode directly");
        }
    }

    let info = usb::wait_for(3, &options.sn, settings.timeout("detect_dev"))?;

    if Mode::from_configuration_index(info.configuration_index) == Some(Mode::Rom) {
        let loader = plan
            .fda
            .as_ref()
            .context("ROM mode requires an FDA image from a PAC")?;
        let mut easyboot = EasybootClient::new(UsbLink::open(info, settings.timeout("default"))?);

        eprintln!("Downloading ROM FDA: {}", loader.display());
        easyboot.download(loader)?;
        drop(easyboot);

        thread::sleep(Duration::from_secs(3));
    }

    let stage = with_fastboot(options, settings.timeout("get_stage"), |client| {
        client.getvar("dev-stage")
    });

    let dloader = match stage {
        Ok(stage) => stage.contains("dloader"),
        Err(error) if error.downcast_ref::<DeviceFailure>().is_some() => false,
        Err(error) => return Err(error.context("query fastboot stage")),
    };

    for image in &plan.images {
        if (plan::is_da(&image.partition) && !dloader) || plan::is_ssb(&image.partition) {
            eprintln!("Bootstrap {}", image.partition);

            with_fastboot(options, settings.timeout(&image.partition), |client| {
                client.flash(&image.partition, &image.path, false)
            })?;

            if image.partition == "emmc1$da" {
                thread::sleep(Duration::from_secs(3));
            }
        }
    }

    Ok(())
}

fn usb_download(options: &Cli, settings: &Config, plan: &Plan, actions: &Actions) -> Result<()> {
    bootstrap(options, settings, plan)?;

    if actions.is_empty() {
        for image in &plan.images {
            if plan::is_da(&image.partition)
                || plan::is_ssb(&image.partition)
                || plan::is_boot(&image.partition)
            {
                continue;
            }

            eprintln!("Flash {}", image.partition);

            with_fastboot(options, settings.timeout(&image.partition), |client| {
                if image.partition.contains("partition") || image.partition.contains("emmc") {
                    client.erase(&image.partition)?;
                }

                client.flash(&image.partition, &image.path, !options.skip_verify)
            })?;
        }

        for partition in &plan.auto_erase {
            with_fastboot(options, settings.timeout("default"), |client| {
                client.erase(partition)
            })?;
        }
    } else {
        for (partition, _) in &actions.erase {
            with_fastboot(options, settings.timeout("default"), |client| {
                client.erase(partition)
            })?;
        }

        for (partition, path) in &actions.flash {
            with_fastboot(options, settings.timeout(partition), |client| {
                if partition.contains("partition")
                    || partition.contains("emmc")
                    || partition.contains("ssd")
                {
                    client.erase(partition)?;
                }

                client.flash(partition, path, !options.skip_verify)
            })?;
        }

        for (partition, path, _) in &actions.read {
            with_fastboot(options, settings.timeout(partition), |client| {
                client.readback(partition, path)
            })?;
        }
    }

    if options.reboot {
        with_fastboot(options, settings.timeout("default"), |client| {
            client.reboot()
        })?;
    }

    Ok(())
}

pub fn execute(options: &Cli) -> Result<()> {
    let actions = options.actions()?;
    let settings = Config::load(options.config.as_deref())?;

    if options.unpac_path.is_none() {
        ensure!(
            actions.erase.iter().all(|(_, size)| *size == 0)
                && actions.read.iter().all(|(_, _, size)| *size == 0),
            "sized read/erase arguments require J-Link, which is not supported"
        );
    }

    let temporary = tempfile::Builder::new().prefix("sddownload-").tempdir()?;
    let temporary = if options.debug {
        let path = temporary.keep();
        eprintln!("Debug images retained in {}", path.display());
        Temporary::Kept(path)
    } else {
        Temporary::Owned(temporary)
    };

    let plan = Plan::prepare(options, &settings, temporary.path())?;

    if options.debug {
        eprintln!("Download plan: {plan:#?}");
    }

    if options.unpac_path.is_some() {
        println!("Unpac successfully");

        return Ok(());
    }

    let elf_loader = plan
        .fda
        .as_ref()
        .map(sdimage::elf::Loader::is_elf)
        .transpose()?
        .unwrap_or(false);

    ensure!(
        !elf_loader,
        "ELF FDA requires J-Link, which is not supported; use --unpac_path to extract it"
    );

    usb_download(options, &settings, &plan, &actions)?;
    println!("Download successfully");

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
            eprintln!("sddownload: {error:#}");
            1
        }
    }
}
