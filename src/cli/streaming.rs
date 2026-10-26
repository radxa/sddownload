//! Forward-only download scheduling. Only deferred/reused payloads are cached.
use super::{progress::FlashProgress, with_fastboot, Actions, Cli};
use crate::{config::Config, plan};
use anyhow::{ensure, Context, Result};
use sdeasyboot::{
    fastboot::DeviceFailure,
    usb::{self, EasybootClient, Mode, UsbLink},
};
use sdimage::{
    pac::{Entry, PacFileType},
    stream::PacStream,
};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::File,
    io::{self, Read, Write},
    path::Path,
    thread,
    time::Duration,
};

#[cfg(any(unix, windows))]
mod pipeline;

trait Target {
    fn connect(&mut self) -> Result<()>;
    fn is_rom(&self) -> bool;
    #[cfg(any(unix, windows))]
    fn has_agent(&mut self) -> Result<bool> {
        self.connect()?;
        Ok(!self.is_rom())
    }
    fn loader(&mut self, reader: &mut dyn Read, size: u64) -> Result<()>;
    fn bootstrap(&mut self, partition: &str, reader: &mut dyn Read, size: u64) -> Result<()>;
    fn flash(&mut self, partition: &str, reader: &mut dyn Read, size: u64) -> Result<()>;
}

struct UsbTarget<'a> {
    options: &'a Cli,
    settings: &'a Config,
    rom: Option<bool>,
    dloader: Option<bool>,
    #[cfg(any(unix, windows))]
    agent_loaded: bool,
}

impl<'a> UsbTarget<'a> {
    fn new(options: &'a Cli, settings: &'a Config) -> Result<Self> {
        Ok(Self {
            options,
            settings,
            rom: None,
            dloader: None,
            #[cfg(any(unix, windows))]
            agent_loaded: false,
        })
    }

    fn stage(&mut self) -> Result<bool> {
        if let Some(dloader) = self.dloader {
            return Ok(dloader);
        }
        let stage = with_fastboot(self.options, self.settings.timeout("get_stage"), |client| {
            client.getvar("dev-stage")
        });
        let dloader = match stage {
            Ok(stage) => stage.contains("dloader"),
            Err(error) if error.downcast_ref::<DeviceFailure>().is_some() => false,
            Err(error) => return Err(error.context("query fastboot stage")),
        };
        self.dloader = Some(dloader);
        Ok(dloader)
    }
}

impl Target for UsbTarget<'_> {
    fn connect(&mut self) -> Result<()> {
        if self.rom.is_some() {
            return Ok(());
        }
        if self.options.automatic {
            let key = self
                .options
                .adb_key
                .clone()
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::PathBuf::from(home).join(".android/adbkey"))
                })
                .context("set --adb-key when HOME is unavailable")?;
            if let Err(error) = sdeasyboot::adb::reboot_bootloader(
                &self.options.sn,
                &key,
                self.settings.timeout("detect_dev"),
            ) {
                eprintln!(
                    "ADB auto-reboot unavailable: {error:#}; detecting download mode directly"
                );
            }
        }
        let info = usb::wait_for(3, &self.options.sn, self.settings.timeout("detect_dev"))?;
        self.rom =
            Some(Mode::from_configuration_index(info.configuration_index) == Some(Mode::Rom));
        Ok(())
    }

    fn is_rom(&self) -> bool {
        self.rom.unwrap_or(true)
    }

    #[cfg(any(unix, windows))]
    fn has_agent(&mut self) -> Result<bool> {
        self.connect()?;
        Ok(!self.is_rom() && (self.agent_loaded || self.stage()?))
    }

    fn loader(&mut self, reader: &mut dyn Read, size: u64) -> Result<()> {
        self.connect()?;
        if !self.is_rom() {
            return Ok(());
        }
        let info = usb::wait_for(3, &self.options.sn, self.settings.timeout("detect_dev"))?;
        ensure!(
            Mode::from_configuration_index(info.configuration_index) == Some(Mode::Rom),
            "USB mode changed before FDA transfer"
        );
        let mut client =
            EasybootClient::new(UsbLink::open(info, self.settings.timeout("default"))?);
        client.download_reader(reader, size)?;
        drop(client);
        thread::sleep(Duration::from_secs(3));
        usb::wait_for_mode(
            &self.options.sn,
            self.settings.timeout("detect_dev"),
            Mode::Fastboot,
        )?;
        self.rom = Some(false);
        Ok(())
    }

    fn bootstrap(&mut self, partition: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
        self.connect()?;
        ensure!(
            !self.is_rom(),
            "ROM mode requires an FDA before download agents"
        );
        if plan::is_da(partition) && self.stage()? {
            return Ok(());
        }
        eprintln!("Bootstrap {partition}");
        with_fastboot(self.options, self.settings.timeout(partition), |client| {
            flash_with_progress(client, partition, reader, size, false)
        })?;
        #[cfg(any(unix, windows))]
        {
            if plan::is_da(partition) {
                self.agent_loaded = true;
            }
        }
        if partition == "emmc1$da" {
            thread::sleep(Duration::from_secs(3));
        }
        Ok(())
    }

    fn flash(&mut self, partition: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
        self.connect()?;
        ensure!(
            !self.is_rom(),
            "ROM mode requires --fda or an FDA image from a PAC"
        );
        self.stage()?;
        with_fastboot(self.options, self.settings.timeout(partition), |client| {
            if partition.contains("partition") || partition.contains("emmc") {
                client.erase(partition)?;
            }
            flash_with_progress(client, partition, reader, size, !self.options.skip_verify)
        })
    }
}

fn flash_with_progress<T: usb::BulkTransport>(
    client: &mut sdeasyboot::fastboot::FastbootClient<T>,
    partition: &str,
    reader: &mut dyn Read,
    size: u64,
    verify: bool,
) -> Result<()> {
    let mut progress = FlashProgress::new(partition);
    let result = client.flash_reader_with_progress(partition, reader, size, verify, |state| {
        progress.update(state);
    });
    progress.finish(result.is_ok());
    result
}

fn loader(entry: &Entry) -> Option<(u8, &'static str)> {
    match (entry.kind, entry.partition.contains("ssb")) {
        (PacFileType::Fda, false) => Some((0, "FDA")),
        (PacFileType::OspiDa, false) => Some((1, "ospi1$da")),
        (PacFileType::Dloader, false) => Some((2, "emmc1$da")),
        (PacFileType::Fda, true) => Some((3, "ssb_fda")),
        (PacFileType::Dloader, true) => Some((4, "ssb_emmc1$da")),
        _ => None,
    }
}

fn overridden_fda(options: &Cli, entry: &Entry) -> bool {
    options.fda.is_some() && loader(entry).is_some_and(|(rank, _)| rank == 0)
}

// A raw stdin FDA has no size header. Stage it to disk to determine its length
// and reject empty/ELF input before any device access, without unbounded RAM.
fn cache_fda(
    directory: &Path,
    debug: bool,
    copy: impl FnOnce(&mut File) -> Result<u64>,
) -> Result<Cached> {
    let mut file = tempfile::Builder::new()
        .prefix("fda-")
        .disable_cleanup(debug)
        .tempfile_in(directory)?;
    if debug {
        eprintln!("FDA cache file: {}", file.path().display());
    }
    let size = copy(file.as_file_mut())?;
    ensure!(size > 0, "FDA image is empty");
    file.flush()?;
    let mut magic = [0; 4];
    let count = size.min(4) as usize;
    file.reopen()?.read_exact(&mut magic[..count])?;
    ensure!(
        count != 4 || magic != *b"\x7fELF",
        "ELF FDA requires J-Link, which is not supported"
    );
    Ok(Cached {
        storage: String::new(),
        entry: Entry {
            partition: "FDA".into(),
            kind: PacFileType::Fda,
            size,
            offset: 0,
            slot_ab: false,
            components: vec!["FDA.bin".into()],
        },
        file: file.into_temp_path(),
        #[cfg(any(unix, windows))]
        progress: None,
    })
}

struct Cached {
    storage: String,
    entry: Entry,
    file: tempfile::TempPath,
    #[cfg(any(unix, windows))]
    progress: Option<std::sync::Arc<pipeline::Progress>>,
}

impl Cached {
    fn reader(&self) -> Result<Box<dyn Read + Send>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            options.share_mode(FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let file = options.open(&self.file)?;
        #[cfg(any(unix, windows))]
        if let Some(progress) = &self.progress {
            return Ok(Box::new(progress.reader(file)));
        }
        Ok(Box::new(file))
    }
}

// Later PAC headers may contain a loader of any rank, including another FDA.
const UNSEEN_LOADERS: u8 = u8::MAX;

fn supersede_loader(cached: &mut VecDeque<Cached>, entry: &Entry) {
    if let Some((rank, _)) = loader(entry) {
        cached.retain(|item| loader(&item.entry).is_none_or(|(old, _)| old != rank));
    }
}

fn defer(cached: &mut VecDeque<Cached>, item: Cached) {
    supersede_loader(cached, &item.entry);
    cached.push_back(item);
}

fn cache(
    storage: &str,
    entry: Entry,
    reader: &mut dyn Read,
    directory: &Path,
    debug: bool,
) -> Result<Cached> {
    let mut file = tempfile::Builder::new()
        .prefix("payload-")
        .disable_cleanup(debug)
        .tempfile_in(directory)?;
    eprintln!("Caching {} ({} bytes)", entry.partition, entry.size);
    if debug {
        eprintln!("Cache file: {}", file.path().display());
    }
    ensure!(
        io::copy(reader, &mut file)? == entry.size,
        "truncated cached payload"
    );
    file.flush()?;
    Ok(Cached {
        storage: storage.into(),
        entry,
        file: file.into_temp_path(),
        #[cfg(any(unix, windows))]
        progress: None,
    })
}

struct Tee<'a> {
    reader: &'a mut dyn Read,
    cache: &'a mut File,
}

impl Read for Tee<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(buffer)?;
        self.cache.write_all(&buffer[..count])?;
        Ok(count)
    }
}

struct Engine<'a, T> {
    target: T,
    directory: &'a Path,
    options: &'a Cli,
    actions: &'a Actions,
    cached: VecDeque<Cached>,
}

impl<T: Target> Engine<'_, T> {
    fn wanted(&self, storage: &str, entry: &Entry) -> bool {
        !overridden_fda(self.options, entry)
            && (loader(entry).is_some()
                || (self.actions.is_empty()
                    && entry.kind != PacFileType::OspiDa
                    && !plan::is_boot(&plan::storage_partition(storage, &entry.partition))))
    }

    fn consume(
        &mut self,
        storage: &str,
        entry: &Entry,
        reader: &mut dyn Read,
        cached: bool,
    ) -> Result<()> {
        if let Some((rank, partition)) = loader(entry) {
            if rank == 0 {
                let mut prefix = vec![0; entry.size.min(4) as usize];
                reader.read_exact(&mut prefix)?;
                ensure!(
                    prefix != b"\x7fELF",
                    "ELF FDA requires J-Link, which is not supported"
                );
                let mut source = io::Cursor::new(prefix).chain(reader);
                eprintln!("ROM FDA: {} bytes", entry.size);
                self.target.loader(&mut source, entry.size)?;
                io::copy(&mut source, &mut io::sink())?;
            } else {
                self.target.bootstrap(partition, reader, entry.size)?;
                io::copy(reader, &mut io::sink())?;
            }
            return Ok(());
        }
        if !self.wanted(storage, entry) {
            return Ok(());
        }
        let partition = plan::storage_partition(storage, &entry.partition);
        if self.options.b_part && entry.slot_ab && !cached {
            let mut file = tempfile::Builder::new()
                .prefix("slot-")
                .disable_cleanup(self.options.debug)
                .tempfile_in(self.directory)?;
            eprintln!(
                "Caching {} for slot B while flashing slot A",
                entry.partition
            );
            let mut tee = Tee {
                reader,
                cache: file.as_file_mut(),
            };
            self.target
                .flash(&format!("{partition}_a"), &mut tee, entry.size)?;
            io::copy(&mut tee, &mut io::sink())?;
            file.flush()?;
            self.target
                .flash(&format!("{partition}_b"), &mut file.reopen()?, entry.size)?;
        } else {
            self.target.flash(
                &if self.options.b_part && entry.slot_ab {
                    format!("{partition}_a")
                } else {
                    partition
                },
                reader,
                entry.size,
            )?;
        }
        Ok(())
    }

    fn replay(&mut self, item: Cached) -> Result<()> {
        let mut source = item.reader()?;
        self.consume(&item.storage, &item.entry, &mut source, true)?;
        if loader(&item.entry).is_none()
            && self.wanted(&item.storage, &item.entry)
            && self.options.b_part
            && item.entry.slot_ab
        {
            self.target.flash(
                &format!(
                    "{}_b",
                    plan::storage_partition(&item.storage, &item.entry.partition)
                ),
                &mut item.reader()?,
                item.entry.size,
            )?;
        }
        Ok(())
    }

    fn flush(&mut self, future: &[u8]) -> Result<()> {
        loop {
            let selected = self
                .cached
                .iter()
                .enumerate()
                .filter_map(|(index, item)| loader(&item.entry).map(|(rank, _)| (rank, index)))
                .min_by_key(|&(rank, index)| {
                    (rank, if rank == 0 { usize::MAX - index } else { index })
                });
            let Some((rank, index)) = selected else { break };
            if future
                .iter()
                .any(|&pending| pending == UNSEEN_LOADERS || pending <= rank)
            {
                return Ok(());
            }
            if rank != 0 {
                self.target.connect()?;
                if self.target.is_rom() {
                    return Ok(());
                }
            }
            let item = self.cached.remove(index).unwrap();
            self.replay(item)?;
        }
        if future.is_empty() && !self.cached.is_empty() {
            self.target.connect()?;
        }
        if future.is_empty() && !self.target.is_rom() {
            while let Some(item) = self.cached.pop_front() {
                self.replay(item)?;
            }
        }
        Ok(())
    }

    fn entry(
        &mut self,
        storage: &str,
        entry: Entry,
        reader: &mut dyn Read,
        future: &[u8],
    ) -> Result<()> {
        if overridden_fda(self.options, &entry) {
            return self.flush(future);
        }
        let mut before = future.to_vec();
        if let Some((rank, _)) = loader(&entry) {
            // Match the seekable plan: the last loader of each kind wins.
            supersede_loader(&mut self.cached, &entry);
            if future.contains(&rank) {
                return Ok(());
            }
            before.push(rank);
        }
        self.flush(&before)?;
        if !self.wanted(storage, &entry) {
            return Ok(());
        }
        if loader(&entry).is_none()
            && future.is_empty()
            && !self.cached.iter().any(|item| loader(&item.entry).is_some())
        {
            self.target.connect()?;
        }
        let blocked = if let Some((rank, _)) = loader(&entry) {
            future
                .iter()
                .any(|&pending| pending == UNSEEN_LOADERS || pending <= rank)
                || self
                    .cached
                    .iter()
                    .filter_map(|item| loader(&item.entry))
                    .any(|(pending, _)| pending < rank)
                || (rank != 0 && self.target.is_rom())
        } else {
            !future.is_empty()
                || self.target.is_rom()
                || self.cached.iter().any(|item| loader(&item.entry).is_some())
        };
        if blocked {
            defer(
                &mut self.cached,
                cache(storage, entry, reader, self.directory, self.options.debug)?,
            );
        } else {
            self.consume(storage, &entry, reader, false)?;
        }
        self.flush(future)
    }
}

pub(super) fn input(path: &Path) -> Result<Box<dyn Read>> {
    if path == Path::new("-") {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;
            return Ok(Box::new(File::from(
                io::stdin().as_handle().try_clone_to_owned()?,
            )));
        }
        #[cfg(not(windows))]
        Ok(Box::new(io::stdin()))
    } else {
        Ok(Box::new(
            File::open(path).with_context(|| format!("open PAC {}", path.display()))?,
        ))
    }
}

pub(super) fn enter_fastboot(options: &Cli, settings: &Config, directory: &Path) -> Result<()> {
    ensure!(
        options.inputs().is_empty(),
        "device listing must not consume PAC inputs"
    );
    if options.fda.is_some() {
        stream_inputs(
            options,
            settings,
            &Actions::default(),
            directory,
            UsbTarget::new(options, settings)?,
        )?;
    } else {
        let mut target = UsbTarget::new(options, settings)?;
        require_fastboot(&mut target)?;
    }
    Ok(())
}

fn require_fastboot(target: &mut impl Target) -> Result<()> {
    target.connect()?;
    ensure!(!target.is_rom(), "device is in ROM/download-loader mode; supply --fda PATH to enter fastboot, or specify a PAC for an offline listing");
    Ok(())
}

fn stage_flash_stdin(
    source: &mut dyn Read,
    directory: &Path,
    debug: bool,
) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("flash-stdin-")
        .disable_cleanup(debug)
        .tempfile_in(directory)?;
    eprintln!("Staging flash image from stdin");
    if debug {
        eprintln!("Flash stdin cache file: {}", file.path().display());
    }
    ensure!(
        io::copy(source, &mut file)? > 0,
        "flash image from stdin is empty"
    );
    file.flush()?;
    Ok(file)
}

fn read_to_stdout<T: usb::BulkTransport>(
    client: &mut sdeasyboot::fastboot::FastbootClient<T>,
    partition: &str,
) -> Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        // File uses the inherited binary handle, avoiding console text handling.
        let mut output = File::from(io::stdout().as_handle().try_clone_to_owned()?);
        client.readback_to(partition, &mut output)
    }
    #[cfg(not(windows))]
    client.readback_to(partition, &mut io::stdout().lock())
}

pub(super) fn download(
    options: &Cli,
    settings: &Config,
    actions: &Actions,
    directory: &Path,
) -> Result<()> {
    let stdin_image = if actions.flash.iter().any(|(_, path)| path == Path::new("-")) {
        Some(stage_flash_stdin(
            &mut input(Path::new("-"))?,
            directory,
            options.debug,
        )?)
    } else {
        None
    };
    let (_target, empty) = stream_inputs(
        options,
        settings,
        actions,
        directory,
        UsbTarget::new(options, settings)?,
    )?;
    if actions.is_empty() {
        for partition in empty {
            with_fastboot(options, settings.timeout("default"), |client| {
                client.erase(&partition)
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
                let mut file = if path == Path::new("-") {
                    stdin_image
                        .as_ref()
                        .context("missing staged flash stdin")?
                        .reopen()?
                } else {
                    File::open(path)?
                };
                let size = file.metadata()?.len();
                flash_with_progress(client, partition, &mut file, size, !options.skip_verify)
            })?;
        }
        for (partition, path) in &actions.read {
            with_fastboot(options, settings.timeout(partition), |client| {
                if path == Path::new("-") {
                    read_to_stdout(client, partition)
                } else {
                    client.readback(partition, path)
                }
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

fn stream_inputs<T: Target>(
    options: &Cli,
    settings: &Config,
    actions: &Actions,
    directory: &Path,
    target: T,
) -> Result<(T, BTreeSet<String>)> {
    options.validate_stdin()?;
    #[cfg(any(unix, windows))]
    if !options.verify_first {
        return pipeline::run(options, settings, actions, directory, target);
    }
    validated_inputs(options, settings, actions, directory, target)
}

fn validated_inputs<T: Target>(
    options: &Cli,
    settings: &Config,
    actions: &Actions,
    directory: &Path,
    target: T,
) -> Result<(T, BTreeSet<String>)> {
    let inputs = options.inputs();
    options.validate_stdin()?;
    let mut engine = Engine {
        target,
        directory,
        options,
        actions,
        cached: VecDeque::new(),
    };
    let mut empty = BTreeSet::new();
    if let Some(path) = &options.fda {
        eprintln!("FDA image: {}", path.display());
        let mut source = input(path).with_context(|| format!("open FDA {}", path.display()))?;
        let fda = cache_fda(directory, options.debug, |writer| {
            Ok(io::copy(&mut source, writer)?)
        })?;
        if options.verify_first {
            engine.cached.push_back(fda);
        } else {
            engine.replay(fda)?;
        }
    }
    for (index, (storage, path)) in inputs.iter().enumerate() {
        eprintln!("Streaming PAC: {}", path.display());
        let mut pac = PacStream::new(input(path)?, !options.skip_verify)?;
        ensure!(
            pac.version == u32::from(*storage == "pac"),
            "--pac expects version 1; storage-specific PAC arguments expect version 0"
        );
        let stage = options.verify_first || index + 1 != inputs.len();
        while let Some(entry) = pac.next_entry()? {
            if overridden_fda(options, &entry) {
                continue;
            }
            if stage {
                if engine.wanted(storage, &entry) {
                    defer(
                        &mut engine.cached,
                        cache(storage, entry, &mut pac, directory, options.debug)?,
                    );
                }
            } else {
                let mut future: Vec<_> = pac
                    .pending_entries()
                    .filter(|entry| !overridden_fda(options, entry))
                    .filter_map(loader)
                    .map(|(rank, _)| rank)
                    .collect();
                // Unknown nested headers can change loader selection/order.
                // Keep their predecessors cached until the metadata is known.
                if pac.has_pending_containers() {
                    future.push(UNSEEN_LOADERS);
                }
                engine.entry(storage, entry, &mut pac, &future)?;
            }
        }
        for partition in &pac.empty {
            if matches!(*storage, "emmc1" | "sd" | "pac") {
                let partition = plan::storage_partition(storage, partition);
                if settings
                    .auto_erase
                    .iter()
                    .any(|name| partition.contains(name))
                {
                    empty.insert(partition);
                }
            }
        }
        eprintln!(
            "PAC consumed{}: {}",
            if options.skip_verify {
                " (CRC skipped)"
            } else {
                " and available CRCs verified"
            },
            path.display()
        );
    }
    if options.verify_first {
        // Strict mode has consumed and verified every PAC before ADB/USB writes.
        for item in &engine.cached {
            if loader(&item.entry).is_some_and(|(rank, _)| rank == 0) {
                let mut magic = [0; 4];
                let count = item.entry.size.min(magic.len() as u64) as usize;
                File::open(&item.file)?.read_exact(&mut magic[..count])?;
                ensure!(
                    count != 4 || magic != *b"\x7fELF",
                    "ELF FDA requires J-Link, which is not supported"
                );
            }
        }
    }
    engine.flush(&[])?;
    ensure!(
        engine.cached.is_empty(),
        "ROM mode requires --fda or an FDA image from a PAC"
    );
    engine.target.connect()?;
    ensure!(
        !engine.target.is_rom(),
        "ROM mode requires --fda or an FDA image from a PAC"
    );
    Ok((engine.target, empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::{cell::Cell, rc::Rc};

    #[derive(Default)]
    struct Peer {
        rom: bool,
        calls: Vec<(String, Vec<u8>)>,
        cache_counts: Vec<usize>,
        directory: std::path::PathBuf,
        connections: Rc<Cell<usize>>,
        received: Rc<Cell<usize>>,
        fail_flash: bool,
    }

    impl Peer {
        fn receive(&mut self, name: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
            self.cache_counts
                .push(std::fs::read_dir(&self.directory)?.count());
            let mut bytes = vec![];
            reader.read_to_end(&mut bytes)?;
            ensure!(bytes.len() as u64 == size, "payload boundary mismatch");
            self.received.set(self.received.get() + 1);
            self.calls.push((name.into(), bytes));
            Ok(())
        }
    }

    impl Target for Peer {
        fn connect(&mut self) -> Result<()> {
            self.connections.set(self.connections.get() + 1);
            Ok(())
        }
        fn is_rom(&self) -> bool {
            self.rom
        }
        fn loader(&mut self, reader: &mut dyn Read, size: u64) -> Result<()> {
            self.receive("FDA", reader, size)?;
            self.rom = false;
            Ok(())
        }
        fn bootstrap(&mut self, partition: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
            self.receive(partition, reader, size)
        }
        fn flash(&mut self, partition: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
            ensure!(!self.fail_flash, "simulated flash failure");
            self.receive(partition, reader, size)
        }
    }

    fn entry(name: &str, kind: u32, size: u64) -> Entry {
        Entry {
            partition: name.into(),
            kind: kind.into(),
            size,
            offset: 0,
            slot_ab: false,
            components: vec![format!("{name}.bin")],
        }
    }

    pub(super) fn packet(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut bytes = vec![0; sdimage::pac::TABLE_END as usize];
        bytes[..4].copy_from_slice(&sdimage::pac::MAGIC.to_le_bytes());
        bytes[528..532].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (index, (name, kind, data)) in entries.iter().enumerate() {
            let at = 1024 + index * 128;
            for (index, word) in name.encode_utf16().enumerate() {
                bytes[at + index * 2..at + index * 2 + 2].copy_from_slice(&word.to_le_bytes());
            }
            let offset = bytes.len() as u64;
            bytes[at + 72..at + 80].copy_from_slice(&(data.len() as u64).to_le_bytes());
            bytes[at + 80..at + 88].copy_from_slice(&offset.to_le_bytes());
            bytes[at + 88..at + 92].copy_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(data);
        }
        let mut crc = sdimage::BZIP2.digest();
        crc.update(&bytes[..1020]);
        crc.update(&bytes[1024..]);
        bytes[1020..1024].copy_from_slice(&crc.finalize().to_le_bytes());
        bytes
    }

    #[test]
    fn flash_stdin_stages_binary_image_and_flashes_exact_payload() {
        use std::collections::VecDeque;
        struct FastbootPeer {
            responses: VecDeque<Vec<u8>>,
            writes: Vec<Vec<u8>>,
        }
        impl usb::BulkTransport for FastbootPeer {
            fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
                let bytes = self.responses.pop_front().context("missing response")?;
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            fn write(&mut self, bytes: &[u8]) -> Result<usize> {
                self.writes.push(bytes.to_vec());
                Ok(bytes.len())
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"a\0\r\n\x1a\xff\x80z";
        let file = stage_flash_stdin(&mut bytes.as_slice(), directory.path(), false).unwrap();
        let mut client = sdeasyboot::fastboot::FastbootClient::new(FastbootPeer {
            responses: [
                b"OKAY0x8".to_vec(),
                b"DATA00000008".to_vec(),
                b"OKAY".to_vec(),
                b"OKAY".to_vec(),
            ]
            .into(),
            writes: vec![],
        });
        flash_with_progress(&mut client, "boot", &mut file.reopen().unwrap(), 8, false).unwrap();
        assert_eq!(client.transport.writes[2], bytes);
        assert_eq!(client.transport.writes[3], b"flash:boot");
        drop(file);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn listing_without_fda_accepts_fastboot_and_explains_rom_mode_without_writes() {
        for rom in [false, true] {
            let mut peer = Peer {
                rom,
                ..Peer::default()
            };
            let result = require_fastboot(&mut peer);
            if rom {
                assert!(result.unwrap_err().to_string().contains("--fda PATH"));
            } else {
                result.unwrap();
            }
            assert_eq!(peer.connections.get(), 1);
            assert!(peer.calls.is_empty());
        }
    }

    #[test]
    fn explicit_fda_alone_bootstraps_without_pac_images() {
        let input = tempfile::tempdir().unwrap();
        let fda = input.path().join("rom.bin");
        let bytes = b"ROM\0\r\n\x1a\xff";
        std::fs::write(&fda, bytes).unwrap();
        for strict in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut options =
                Cli::try_parse_from(["sddownload", "--fda", fda.to_str().unwrap()]).unwrap();
            options.verify_first = strict;
            assert!(options.actions().unwrap().is_empty());
            let peer = Peer {
                rom: true,
                directory: directory.path().into(),
                ..Peer::default()
            };
            let (peer, empty) = stream_inputs(
                &options,
                &Config::load(None).unwrap(),
                &Actions::default(),
                directory.path(),
                peer,
            )
            .unwrap();
            assert_eq!(peer.calls, [("FDA".into(), bytes.to_vec())]);
            assert!(!peer.rom);
            assert!(empty.is_empty());
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn explicit_fda_overrides_all_primary_pac_loaders_including_nested_elf() {
        let input = tempfile::tempdir().unwrap();
        let fda = input.path().join("rom.bin");
        let first = input.path().join("first.pac");
        let second = input.path().join("second.pac");
        std::fs::write(&fda, b"override").unwrap();
        std::fs::write(&first, packet(&[("FDA", 0, b"old")])).unwrap();
        let nested = packet(&[("FDA", 0, b"\x7fELFignored"), ("kernel", 6, b"image")]);
        std::fs::write(
            &second,
            packet(&[
                ("da", 2, b"agent"),
                ("nested", 7, &nested),
                ("FDA", 0, b"conflicting late FDA"),
            ]),
        )
        .unwrap();
        for strict in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut options = Cli::try_parse_from([
                "sddownload",
                "--fda",
                fda.to_str().unwrap(),
                "--ospi1",
                first.to_str().unwrap(),
                "--emmc1",
                second.to_str().unwrap(),
            ])
            .unwrap();
            options.verify_first = strict;
            let peer = Peer {
                rom: true,
                directory: directory.path().into(),
                ..Peer::default()
            };
            let (peer, _) = stream_inputs(
                &options,
                &Config::load(None).unwrap(),
                &Actions::default(),
                directory.path(),
                peer,
            )
            .unwrap();
            assert_eq!(
                peer.calls,
                [
                    ("FDA".into(), b"override".to_vec()),
                    ("emmc1$da".into(), b"agent".to_vec()),
                    ("emmc1$nested$kernel".into(), b"image".to_vec()),
                ]
            );
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn explicit_fda_is_validated_before_any_device_access() {
        let input = tempfile::tempdir().unwrap();
        let fda = input.path().join("rom.bin");
        for bytes in [b"".as_slice(), b"\x7fELFinvalid"] {
            std::fs::write(&fda, bytes).unwrap();
            for strict in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut options =
                    Cli::try_parse_from(["sddownload", "--fda", fda.to_str().unwrap()]).unwrap();
                options.verify_first = strict;
                let connections = Rc::new(Cell::new(0));
                let peer = Peer {
                    rom: true,
                    directory: directory.path().into(),
                    connections: connections.clone(),
                    ..Peer::default()
                };
                let error = stream_inputs(
                    &options,
                    &Config::load(None).unwrap(),
                    &Actions::default(),
                    directory.path(),
                    peer,
                )
                .err()
                .unwrap();
                assert!(
                    error.to_string().contains(if bytes.is_empty() {
                        "FDA image is empty"
                    } else {
                        "ELF FDA"
                    }),
                    "{error:#}"
                );
                assert_eq!(connections.get(), 0);
                assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn verify_first_bad_pac_crc_does_not_send_explicit_fda() {
        let input = tempfile::tempdir().unwrap();
        let fda = input.path().join("rom.bin");
        let pac = input.path().join("bad.pac");
        std::fs::write(&fda, b"override").unwrap();
        let mut bytes = packet(&[("FDA", 0, b"ignored"), ("kernel", 6, b"image")]);
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&pac, bytes).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--fda",
            fda.to_str().unwrap(),
            "--emmc1",
            pac.to_str().unwrap(),
            "--verify-first",
        ])
        .unwrap();
        let connections = Rc::new(Cell::new(0));
        let peer = Peer {
            rom: true,
            directory: directory.path().into(),
            connections: connections.clone(),
            ..Peer::default()
        };
        let error = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            peer,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("CRC mismatch"), "{error:#}");
        assert_eq!(connections.get(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn nested_headers_cannot_bootstrap_agents_out_of_order() {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("nested.pac");
        let first = packet(&[("da", 2, b"emmc"), ("kernel", 6, b"image")]);
        let second = packet(&[("da", 1, b"ospi")]);
        std::fs::write(
            &path,
            packet(&[("first", 7, &first), ("second", 7, &second)]),
        )
        .unwrap();
        for strict in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut options =
                Cli::try_parse_from(["sddownload", "--emmc1", path.to_str().unwrap()]).unwrap();
            options.verify_first = strict;
            let peer = Peer {
                directory: directory.path().into(),
                ..Peer::default()
            };
            let result = stream_inputs(
                &options,
                &Config::load(None).unwrap(),
                &Actions::default(),
                directory.path(),
                peer,
            );
            if !strict && cfg!(any(unix, windows)) {
                let error = result.err().unwrap().to_string();
                assert!(
                    error.contains("late bootstrap loader") && error.contains("--verify-first")
                );
                assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
                continue;
            }
            let (peer, _) = result.unwrap();
            assert_eq!(peer.calls[0], ("ospi1$da".into(), b"ospi".to_vec()));
            assert_eq!(peer.calls[1], ("emmc1$da".into(), b"emmc".to_vec()));
            assert_eq!(peer.calls[2].1, b"image");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn multiple_inputs_select_only_the_latest_loaders_and_reuse_ab_cache() {
        let input = tempfile::tempdir().unwrap();
        let first = input.path().join("first.pac");
        let second = input.path().join("second.pac");
        std::fs::write(
            &first,
            packet(&[("FDA", 0, b"\x7fELFobsolete"), ("da", 2, b"old")]),
        )
        .unwrap();
        std::fs::write(
            &second,
            packet(&[
                ("FDA", 0, b"ROM"),
                ("da", 2, b"new"),
                ("kernel_a", 6, b"image"),
                ("misc", 6, b""),
            ]),
        )
        .unwrap();
        for strict in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut options = Cli::try_parse_from([
                "sddownload",
                "--ospi1",
                first.to_str().unwrap(),
                "--emmc1",
                second.to_str().unwrap(),
                "--b-part",
            ])
            .unwrap();
            options.verify_first = strict;
            let peer = Peer {
                rom: true,
                directory: directory.path().into(),
                ..Peer::default()
            };
            let result = stream_inputs(
                &options,
                &Config::load(None).unwrap(),
                &Actions::default(),
                directory.path(),
                peer,
            );
            if !strict && cfg!(any(unix, windows)) {
                assert!(result.err().unwrap().to_string().contains("ELF FDA"));
                assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
                continue;
            }
            let (peer, empty) = result.unwrap();
            assert_eq!(
                peer.calls,
                [
                    ("FDA".into(), b"ROM".to_vec()),
                    ("emmc1$da".into(), b"new".to_vec()),
                    ("emmc1$$kernel_a".into(), b"image".to_vec()),
                    ("emmc1$$kernel_b".into(), b"image".to_vec()),
                ]
            );
            assert_eq!(empty, BTreeSet::from(["emmc1$$misc".to_string()]));
            assert_eq!(peer.cache_counts[2], 1);
            assert_eq!(peer.cache_counts[3], 1);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn crc_policy_controls_device_access_and_cleans_failed_cache() {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("corrupt.pac");
        let mut bytes = packet(&[("kernel", 6, b"image")]);
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&path, bytes).unwrap();
        for strict in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut options =
                Cli::try_parse_from(["sddownload", "--emmc1", path.to_str().unwrap(), "--reboot"])
                    .unwrap();
            options.verify_first = strict;
            let peer = Peer {
                directory: directory.path().into(),
                ..Peer::default()
            };
            let connections = peer.connections.clone();
            let received = peer.received.clone();
            let error = stream_inputs(
                &options,
                &Config::load(None).unwrap(),
                &Actions::default(),
                directory.path(),
                peer,
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("CRC mismatch"));
            if strict {
                assert_eq!(received.get(), 0);
                assert_eq!(connections.get(), 0);
            } else {
                // The input worker can detect a tiny corrupt PAC before USB
                // consumes it; normal mode permits, but does not require, writes.
                assert!(received.get() <= 1);
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn strict_elf_loader_validation_precedes_device_discovery() {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("elf.pac");
        std::fs::write(&path, packet(&[("FDA", 0, b"\x7fELFunsupported")])).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--emmc1",
            path.to_str().unwrap(),
            "--verify-first",
        ])
        .unwrap();
        let peer = Peer {
            rom: true,
            directory: directory.path().into(),
            ..Peer::default()
        };
        let connections = peer.connections.clone();
        let error = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            peer,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ELF FDA"));
        assert_eq!(connections.get(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn device_failure_removes_cache_unless_debug_retention_is_enabled() {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("valid.pac");
        std::fs::write(&path, packet(&[("kernel", 6, b"image")])).unwrap();
        for debug in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut options = Cli::try_parse_from([
                "sddownload",
                "--emmc1",
                path.to_str().unwrap(),
                "--verify-first",
            ])
            .unwrap();
            options.debug = debug;
            let peer = Peer {
                fail_flash: true,
                directory: directory.path().into(),
                ..Peer::default()
            };
            let error = stream_inputs(
                &options,
                &Config::load(None).unwrap(),
                &Actions::default(),
                directory.path(),
                peer,
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("simulated flash failure"));
            assert_eq!(
                std::fs::read_dir(directory.path()).unwrap().count(),
                usize::from(debug)
            );
        }
    }

    #[test]
    fn complete_flat_pac_streams_without_payload_cache() {
        let input_directory = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = input_directory.path().join("images.pac");
        std::fs::write(
            &path,
            packet(&[
                ("kernel", 6, b"abc"),
                ("rootfs", 6, b"def"),
                ("misc", 6, b""),
            ]),
        )
        .unwrap();
        let options =
            Cli::try_parse_from(["sddownload", "--emmc1", path.to_str().unwrap()]).unwrap();
        let (peer, empty) = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                directory: directory.path().into(),
                ..Peer::default()
            },
        )
        .unwrap();
        assert_eq!(
            peer.calls,
            [
                ("emmc1$$kernel".into(), b"abc".to_vec()),
                ("emmc1$$rootfs".into(), b"def".to_vec())
            ]
        );
        assert_eq!(peer.cache_counts, [0, 0]);
        assert_eq!(empty, BTreeSet::from(["emmc1$$misc".into()]));
    }

    #[test]
    fn verify_first_nested_headers_select_the_last_copy() {
        let input_directory = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = input_directory.path().join("nested.pac");
        let first = packet(&[("emmc-da", 2, b"old")]);
        let last = packet(&[
            ("FDA", 0, b"ROM"),
            ("ospi-da", 1, b"ospi"),
            ("emmc-da", 2, b"new"),
        ]);
        std::fs::write(
            &path,
            packet(&[
                ("first", 7, &first),
                ("last", 7, &last),
                ("kernel", 6, b"payload"),
            ]),
        )
        .unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--emmc1",
            path.to_str().unwrap(),
            "--verify-first",
        ])
        .unwrap();
        let (peer, _) = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                directory: directory.path().into(),
                ..Peer::default()
            },
        )
        .unwrap();
        assert_eq!(
            peer.calls,
            [
                ("FDA".into(), b"ROM".to_vec()),
                ("ospi1$da".into(), b"ospi".to_vec()),
                ("emmc1$da".into(), b"new".to_vec()),
                ("emmc1$$kernel".into(), b"payload".to_vec())
            ]
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn verify_first_loader_supersedes_a_copy_from_an_earlier_input() {
        let input_directory = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ospi = input_directory.path().join("ospi.pac");
        let emmc = input_directory.path().join("emmc.pac");
        std::fs::write(&ospi, packet(&[("da", 1, b"old")])).unwrap();
        std::fs::write(
            &emmc,
            packet(&[("da", 1, b"new"), ("kernel", 6, b"payload")]),
        )
        .unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--ospi1",
            ospi.to_str().unwrap(),
            "--emmc1",
            emmc.to_str().unwrap(),
            "--verify-first",
        ])
        .unwrap();
        let (peer, _) = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                directory: directory.path().into(),
                ..Peer::default()
            },
        )
        .unwrap();
        assert_eq!(
            peer.calls,
            [
                ("ospi1$da".into(), b"new".to_vec()),
                ("emmc1$$kernel".into(), b"payload".to_vec())
            ]
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn verify_first_replays_only_the_latest_loaders_and_reuses_ab_cache() {
        let input_directory = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ospi = input_directory.path().join("ospi.pac");
        let emmc = input_directory.path().join("emmc.pac");
        // The superseded ELF must not reject an otherwise supported download.
        std::fs::write(
            &ospi,
            packet(&[("FDA", 0, b"\x7fELFold"), ("da", 1, b"old")]),
        )
        .unwrap();
        std::fs::write(
            &emmc,
            packet(&[
                ("FDA", 0, b"ROM"),
                ("ospi-da", 1, b"ospi"),
                ("emmc-da", 2, b"emmc"),
                ("kernel_a", 6, b"payload"),
            ]),
        )
        .unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--ospi1",
            ospi.to_str().unwrap(),
            "--emmc1",
            emmc.to_str().unwrap(),
            "--verify-first",
            "--b-part",
        ])
        .unwrap();
        let (peer, _) = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                rom: true,
                directory: directory.path().into(),
                ..Peer::default()
            },
        )
        .unwrap();
        assert_eq!(
            peer.calls,
            [
                ("FDA".into(), b"ROM".to_vec()),
                ("ospi1$da".into(), b"ospi".to_vec()),
                ("emmc1$da".into(), b"emmc".to_vec()),
                ("emmc1$$kernel_a".into(), b"payload".to_vec()),
                ("emmc1$$kernel_b".into(), b"payload".to_vec())
            ]
        );
        assert_eq!(peer.cache_counts[3], peer.cache_counts[4]);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn verify_first_crc_failure_has_no_device_access_and_cleans_cache() {
        let input_directory = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ospi = input_directory.path().join("ospi.pac");
        let emmc = input_directory.path().join("emmc.pac");
        std::fs::write(&ospi, packet(&[("FDA", 0, b"ROM")])).unwrap();
        let mut bytes = packet(&[("kernel", 6, b"payload")]);
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&emmc, bytes).unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--ospi1",
            ospi.to_str().unwrap(),
            "--emmc1",
            emmc.to_str().unwrap(),
            "--verify-first",
            "--auto",
            "--reboot",
        ])
        .unwrap();
        let connections = Rc::new(Cell::new(0));
        let error = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                directory: directory.path().into(),
                connections: connections.clone(),
                ..Peer::default()
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("CRC mismatch"));
        assert_eq!(connections.get(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn verify_first_elf_rejection_precedes_device_access() {
        let input_directory = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = input_directory.path().join("images.pac");
        std::fs::write(
            &path,
            packet(&[("kernel", 6, b"payload"), ("FDA", 0, b"\x7fELFloader")]),
        )
        .unwrap();
        let options = Cli::try_parse_from([
            "sddownload",
            "--emmc1",
            path.to_str().unwrap(),
            "--verify-first",
        ])
        .unwrap();
        let connections = Rc::new(Cell::new(0));
        let error = stream_inputs(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                directory: directory.path().into(),
                connections: connections.clone(),
                ..Peer::default()
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ELF FDA"));
        assert_eq!(connections.get(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn single_use_payloads_stream_without_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
        let actions = Actions::default();
        let peer = Peer {
            directory: directory.path().into(),
            ..Peer::default()
        };
        let mut engine = Engine {
            target: peer,
            directory: directory.path(),
            options: &options,
            actions: &actions,
            cached: VecDeque::new(),
        };
        engine
            .entry("emmc1", entry("kernel", 6, 3), &mut b"abc".as_slice(), &[])
            .unwrap();
        engine
            .entry("emmc1", entry("rootfs", 6, 3), &mut b"def".as_slice(), &[])
            .unwrap();
        assert_eq!(engine.target.cache_counts, [0, 0]);
        assert_eq!(
            engine.target.calls[0],
            ("emmc1$$kernel".into(), b"abc".to_vec())
        );
        assert_eq!(
            engine.target.calls[1],
            ("emmc1$$rootfs".into(), b"def".to_vec())
        );
    }

    #[test]
    fn deferred_images_and_agents_replay_in_bootstrap_order() {
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
        let actions = Actions::default();
        let peer = Peer {
            rom: true,
            directory: directory.path().into(),
            ..Peer::default()
        };
        let mut engine = Engine {
            target: peer,
            directory: directory.path(),
            options: &options,
            actions: &actions,
            cached: VecDeque::new(),
        };
        engine
            .entry(
                "emmc1",
                entry("spl", 3, 3),
                &mut b"spl".as_slice(),
                &[0, 1, 2],
            )
            .unwrap();
        engine
            .entry("emmc1", entry("da", 2, 4), &mut b"emmc".as_slice(), &[0, 1])
            .unwrap();
        assert!(engine.target.calls.is_empty());
        engine
            .entry("emmc1", entry("FDA", 0, 3), &mut b"ROM".as_slice(), &[1])
            .unwrap();
        engine
            .entry(
                "emmc1",
                entry("ospi-da", 1, 4),
                &mut b"ospi".as_slice(),
                &[],
            )
            .unwrap();
        let names: Vec<_> = engine
            .target
            .calls
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["FDA", "ospi1$da", "emmc1$da", "emmc1$$spl"]);
        assert!(engine.cached.is_empty());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn agent_waits_for_fda_discovered_in_a_later_container() {
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
        let actions = Actions::default();
        let peer = Peer {
            rom: true,
            directory: directory.path().into(),
            ..Peer::default()
        };
        let mut engine = Engine {
            target: peer,
            directory: directory.path(),
            options: &options,
            actions: &actions,
            cached: VecDeque::new(),
        };
        engine
            .entry("emmc1", entry("da", 2, 4), &mut b"emmc".as_slice(), &[])
            .unwrap();
        assert!(engine.target.calls.is_empty());
        engine
            .entry("emmc1", entry("FDA", 0, 3), &mut b"ROM".as_slice(), &[])
            .unwrap();
        assert_eq!(
            engine
                .target
                .calls
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["FDA", "emmc1$da"]
        );
        assert!(engine.cached.is_empty());
    }

    #[test]
    fn bad_pac_crc_is_reported_after_the_streamed_payload_was_written() {
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
        let actions = Actions::default();
        let peer = Peer {
            directory: directory.path().into(),
            ..Peer::default()
        };
        let mut engine = Engine {
            target: peer,
            directory: directory.path(),
            options: &options,
            actions: &actions,
            cached: VecDeque::new(),
        };
        let mut bytes = vec![0; sdimage::pac::TABLE_END as usize];
        bytes[..4].copy_from_slice(&sdimage::pac::MAGIC.to_le_bytes());
        bytes[528..532].copy_from_slice(&1u32.to_le_bytes());
        bytes[1024..1026].copy_from_slice(&[b'x', 0]);
        bytes[1096..1104].copy_from_slice(&3u64.to_le_bytes());
        bytes[1104..1112].copy_from_slice(&sdimage::pac::TABLE_END.to_le_bytes());
        bytes[1112..1116].copy_from_slice(&6u32.to_le_bytes());
        bytes[1020..1024].copy_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(b"abc");
        let mut pac = PacStream::new(bytes.as_slice(), true).unwrap();
        let entry = pac.next_entry().unwrap().unwrap();
        engine.entry("emmc1", entry, &mut pac, &[]).unwrap();
        assert_eq!(engine.target.calls, [("emmc1$$x".into(), b"abc".to_vec())]);
        assert!(pac
            .next_entry()
            .unwrap_err()
            .to_string()
            .contains("CRC mismatch"));
    }

    #[test]
    fn ab_payload_is_cached_during_first_flash_then_replayed_and_removed() {
        let directory = tempfile::tempdir().unwrap();
        let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused", "--b-part"]).unwrap();
        let actions = Actions::default();
        let peer = Peer {
            directory: directory.path().into(),
            ..Peer::default()
        };
        let mut engine = Engine {
            target: peer,
            directory: directory.path(),
            options: &options,
            actions: &actions,
            cached: VecDeque::new(),
        };
        let mut image = entry("kernel", 6, 7);
        image.slot_ab = true;
        engine
            .entry("emmc1", image, &mut b"payload".as_slice(), &[])
            .unwrap();
        assert_eq!(
            engine.target.calls,
            [
                ("emmc1$$kernel_a".into(), b"payload".to_vec()),
                ("emmc1$$kernel_b".into(), b"payload".to_vec())
            ]
        );
        assert_eq!(engine.target.cache_counts, [1, 1]);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
