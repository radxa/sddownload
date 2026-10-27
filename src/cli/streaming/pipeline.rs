//! Concurrent input producer and serial USB consumer. No USB handle crosses threads.
use super::*;
use anyhow::bail;
use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
        Arc, Condvar, Mutex,
    },
};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix::open_input;
#[cfg(all(test, unix))]
use unix::Input;
#[cfg(windows)]
use windows::open_input;
#[cfg(all(test, windows))]
use windows::Input;

const CHUNK: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(50);
const INPUT_FAILED: u8 = 1;
const USB_FAILED: u8 = 2;
const BUSY: u8 = 0;
const FLAT_READY: u8 = 1;
const AGENT_READY: u8 = 2;

#[derive(Default)]
struct Stop(AtomicU8, #[cfg(windows)] windows::Cancellation);

impl Stop {
    fn cancel(&self, origin: u8) {
        let _ = self
            .0
            .compare_exchange(0, origin, Ordering::AcqRel, Ordering::Acquire);
        #[cfg(windows)]
        self.1.cancel();
    }

    fn check(&self) -> io::Result<()> {
        if self.0.load(Ordering::Acquire) == 0 {
            Ok(())
        } else {
            Err(io::Error::other("streaming download cancelled"))
        }
    }

    fn register_input(&self) -> io::Result<()> {
        #[cfg(windows)]
        self.1.register()?;
        self.check()
    }

    fn wait_for_input(&self) {
        #[cfg(windows)]
        if self.0.load(Ordering::Acquire) != 0 {
            self.1.cancel_until_finished();
        }
    }
}

struct CancelOnDrop {
    stop: Option<Arc<Stop>>,
    receiver: Option<Receiver<Work>>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.cancel(USB_FAILED);
            // Release bounded senders before waiting for the worker. During a
            // panic the receiver would otherwise outlive this guard's Drop.
            drop(self.receiver.take());
            stop.wait_for_input();
        }
    }
}

pub(super) struct Progress {
    state: Mutex<(u64, bool)>,
    changed: Condvar,
    stop: Arc<Stop>,
}

impl Progress {
    fn publish(&self, available: u64) {
        self.state.lock().unwrap().0 = available;
        self.changed.notify_all();
    }

    fn finish(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }

    pub(super) fn reader(self: &Arc<Self>, file: File) -> GrowingReader {
        GrowingReader {
            file,
            position: 0,
            progress: self.clone(),
        }
    }
}

pub(super) struct GrowingReader {
    file: File,
    position: u64,
    progress: Arc<Progress>,
}

impl Read for GrowingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut state = self.progress.state.lock().unwrap();
        loop {
            self.progress.stop.check()?;
            if self.position < state.0 {
                let count = (state.0 - self.position).min(buffer.len() as u64) as usize;
                drop(state);
                let count = self.file.read(&mut buffer[..count])?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "cache was truncated",
                    ));
                }
                self.position += count as u64;
                return Ok(count);
            }
            if state.1 {
                return Ok(0);
            }
            state = self.progress.changed.wait_timeout(state, POLL).unwrap().0;
        }
    }
}

struct Pipe {
    receiver: Receiver<Vec<u8>>,
    current: io::Cursor<Vec<u8>>,
    stop: Arc<Stop>,
    expected: u64,
    consumed: u64,
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if self.consumed != self.expected {
            // Publish the consumer failure before dropping the receiver wakes
            // a blocked sender; its BrokenPipe is secondary, not an input error.
            self.stop.cancel(USB_FAILED);
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            self.stop.check()?;
            let count = self.current.read(buffer)?;
            if count != 0 {
                self.consumed += count as u64;
                return Ok(count);
            }
            match self.receiver.recv_timeout(POLL) {
                Ok(bytes) => self.current = io::Cursor::new(bytes),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
    }
}

enum Payload {
    Cached(Cached),
    Direct {
        storage: String,
        entry: Entry,
        reader: Pipe,
    },
}

enum Work {
    Entry {
        payload: Payload,
        future: Vec<u8>,
        unknown: bool,
    },
    Done(BTreeSet<String>),
}

fn copy_entry(
    reader: &mut dyn Read,
    stop: &Stop,
    mut write: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let mut buffer = vec![0; CHUNK];
    let mut total = 0;
    loop {
        stop.check()?;
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(count) => {
                write(&buffer[..count])?;
                total += count as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_cache(writer: &mut impl Write, mut bytes: &[u8], stop: &Stop) -> io::Result<()> {
    // Unlike Write::write_all, check cancellation between interrupted writes.
    // Windows cancellation can interrupt the producer's cache writes as well.
    while !bytes.is_empty() {
        stop.check()?;
        match writer.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn produce(
    options: &Cli,
    settings: &Config,
    actions: &Actions,
    directory: &Path,
    sender: &SyncSender<Work>,
    mode: &AtomicU8,
    in_flight: &AtomicUsize,
    stop: &Arc<Stop>,
    mut open: impl FnMut(&Path, &Arc<Stop>) -> Result<Box<dyn Read + Send>>,
) -> Result<()> {
    let mut empty = BTreeSet::new();
    let mut previous_direct = false;
    if let Some(path) = &options.fda {
        stop.check()?;
        sdeasyboot::statusln!("FDA image: {}", path.display());
        let mut source =
            open(path, stop).with_context(|| format!("open FDA {}", path.display()))?;
        let fda = cache_fda(directory, options.debug, |writer| {
            copy_entry(&mut source, stop, |bytes| {
                Ok(write_cache(writer, bytes, stop)?)
            })
        })?;
        in_flight.fetch_add(1, Ordering::AcqRel);
        sender
            .send(Work::Entry {
                payload: Payload::Cached(fda),
                future: vec![],
                unknown: false,
            })
            .map_err(|_| anyhow::anyhow!("USB worker stopped"))?;
    }
    for (storage, path) in options.inputs() {
        stop.check()?;
        sdeasyboot::statusln!("Streaming PAC: {}", path.display());
        let mut pac = PacStream::new(open(path, stop)?, !options.skip_verify)?;
        ensure!(
            pac.version == u32::from(storage == "pac"),
            "--pac expects version 1; storage-specific PAC arguments expect version 0"
        );
        while let Some(entry) = pac.next_entry()? {
            stop.check()?;
            if overridden_fda(options, &entry) {
                continue;
            }
            if loader(&entry).is_none()
                && (!actions.is_empty()
                    || entry.kind == PacFileType::OspiDa
                    || plan::is_boot(&plan::storage_partition(storage, &entry.partition)))
            {
                continue;
            }
            let future: Vec<_> = pac
                .pending_entries()
                .filter(|entry| !overridden_fda(options, entry))
                .filter_map(loader)
                .map(|(rank, _)| rank)
                .collect();
            let unknown = pac.has_pending_containers();
            let ready = mode.swap(BUSY, Ordering::AcqRel);
            let direct = loader(&entry).is_none()
                && future.is_empty()
                && (!unknown || ready == AGENT_READY)
                && ((ready != BUSY && in_flight.load(Ordering::Acquire) == 0) || previous_direct);
            let size = entry.size;
            in_flight.fetch_add(1, Ordering::AcqRel);
            if direct {
                let (data_sender, data_receiver) = mpsc::sync_channel(4);
                let reader = Pipe {
                    receiver: data_receiver,
                    current: io::Cursor::new(vec![]),
                    stop: stop.clone(),
                    expected: size,
                    consumed: 0,
                };
                sender
                    .send(Work::Entry {
                        payload: Payload::Direct {
                            storage: storage.into(),
                            entry,
                            reader,
                        },
                        future,
                        unknown,
                    })
                    .map_err(|_| anyhow::anyhow!("USB worker stopped"))?;
                let result = copy_entry(&mut pac, stop, |bytes| {
                    data_sender
                        .send(bytes.to_vec())
                        .map_err(|_| anyhow::anyhow!("USB payload reader stopped"))
                })
                .and_then(|count| {
                    ensure!(count == size, "truncated PAC payload");
                    Ok(())
                });
                if result.is_err() {
                    stop.cancel(INPUT_FAILED);
                }
                drop(data_sender);
                result?;
            } else {
                let file = tempfile::Builder::new()
                    .prefix("payload-")
                    .disable_cleanup(options.debug)
                    .tempfile_in(directory)?;
                sdeasyboot::statusln!("Caching {} ({} bytes)", entry.partition, size);
                if options.debug {
                    sdeasyboot::statusln!("Cache file: {}", file.path().display());
                }
                let (mut writer, file) = file.into_parts();
                let progress = Arc::new(Progress {
                    state: Mutex::new((0, false)),
                    changed: Condvar::new(),
                    stop: stop.clone(),
                });
                let cached = Cached {
                    storage: storage.into(),
                    entry,
                    file,
                    progress: Some(progress.clone()),
                };
                sender
                    .send(Work::Entry {
                        payload: Payload::Cached(cached),
                        future,
                        unknown,
                    })
                    .map_err(|_| anyhow::anyhow!("USB worker stopped"))?;
                let mut written = 0;
                let result = copy_entry(&mut pac, stop, |bytes| {
                    write_cache(&mut writer, bytes, stop)?;
                    written += bytes.len() as u64;
                    progress.publish(written);
                    Ok(())
                })
                .and_then(|count| {
                    ensure!(count == size, "truncated cached payload");
                    writer.flush()?;
                    Ok(())
                });
                if result.is_err() {
                    stop.cancel(INPUT_FAILED);
                }
                progress.finish();
                result?;
            }
            previous_direct = direct;
        }
        for partition in &pac.empty {
            if matches!(storage, "emmc1" | "sd" | "pac") {
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
        sdeasyboot::statusln!(
            "PAC consumed{}: {}",
            if options.skip_verify {
                " (CRC skipped)"
            } else {
                " and available CRCs verified"
            },
            path.display()
        );
    }
    sender
        .send(Work::Done(empty))
        .map_err(|_| anyhow::anyhow!("USB worker stopped"))?;
    Ok(())
}

struct Early<'a, T> {
    engine: Engine<'a, T>,
    committed: BTreeMap<u8, Cached>,
    images_started: bool,
    stop: Arc<Stop>,
}

fn identical(left: &Cached, right: &Cached) -> Result<bool> {
    if left.entry.size != right.entry.size {
        return Ok(false);
    }
    let mut left_reader = left.reader()?;
    let mut right_reader = right.reader()?;
    let mut left_buffer = vec![0; CHUNK];
    let mut right_buffer = vec![0; CHUNK];
    let mut remaining = left.entry.size;
    while remaining != 0 {
        let count = remaining.min(CHUNK as u64) as usize;
        left_reader.read_exact(&mut left_buffer[..count])?;
        right_reader.read_exact(&mut right_buffer[..count])?;
        if left_buffer[..count] != right_buffer[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

impl<T: Target> Early<'_, T> {
    fn can_flash(&mut self, future: &[u8], unknown: bool) -> Result<bool> {
        if future.iter().any(|rank| !self.committed.contains_key(rank))
            || self
                .engine
                .cached
                .iter()
                .any(|item| loader(&item.entry).is_some())
        {
            return Ok(false);
        }
        self.engine.target.connect()?;
        Ok(!self.engine.target.is_rom() && (!unknown || self.engine.target.has_agent()?))
    }

    fn direct_mode(&mut self) -> Result<u8> {
        if !self.engine.cached.is_empty() {
            return Ok(BUSY);
        }
        if self.committed.is_empty() && !self.images_started {
            return Ok(FLAT_READY);
        }
        Ok(if self.engine.target.has_agent()? {
            AGENT_READY
        } else {
            FLAT_READY
        })
    }

    fn flush(&mut self, future: &[u8], unknown: bool) -> Result<()> {
        loop {
            self.stop.check()?;
            let selected = self
                .engine
                .cached
                .iter()
                .enumerate()
                .filter_map(|(index, item)| loader(&item.entry).map(|(rank, _)| (rank, index)))
                .min_by_key(|&(rank, _)| rank);
            let Some((rank, index)) = selected else { break };
            if future
                .iter()
                .any(|pending| *pending < rank && !self.committed.contains_key(pending))
            {
                return Ok(());
            }
            if rank != 0 {
                self.engine.target.connect()?;
                if self.engine.target.is_rom() {
                    return Ok(());
                }
            }
            ensure!(self.committed.keys().all(|previous| *previous < rank),
                "late bootstrap loader {} would change the committed bootstrap order; retry with --verify-first", loader(&self.engine.cached[index].entry).unwrap().1);
            let item = self.engine.cached.remove(index).unwrap();
            let mut source = item.reader()?;
            self.stop.check()?;
            self.engine
                .consume(&item.storage, &item.entry, &mut source, true)?;
            self.committed.insert(rank, item);
        }
        if self.engine.cached.is_empty() || !self.can_flash(future, unknown)? {
            return Ok(());
        }
        while let Some(item) = self.engine.cached.pop_front() {
            self.stop.check()?;
            self.images_started = true;
            self.engine.replay(item)?;
        }
        Ok(())
    }

    fn accept(&mut self, payload: Payload, future: &[u8], unknown: bool) -> Result<()> {
        self.stop.check()?;
        match payload {
            Payload::Cached(item) => {
                if let Some((rank, name)) = loader(&item.entry) {
                    if let Some(previous) = self.committed.get(&rank) {
                        ensure!(identical(previous, &item)?,
                            "conflicting late {name}: an earlier loader was already selected; retry with --verify-first");
                        return self.flush(future, unknown);
                    }
                }
                defer(&mut self.engine.cached, item);
            }
            Payload::Direct {
                storage,
                entry,
                mut reader,
            } => {
                self.flush(future, unknown)?;
                if self.can_flash(future, unknown)? {
                    self.stop.check()?;
                    self.images_started = true;
                    self.engine.consume(&storage, &entry, &mut reader, false)?;
                } else {
                    let item = cache(
                        &storage,
                        entry,
                        &mut reader,
                        self.engine.directory,
                        self.engine.options.debug,
                    )?;
                    defer(&mut self.engine.cached, item);
                }
            }
        }
        self.flush(future, unknown)
    }
}

fn consume<T: Target>(
    early: &mut Early<'_, T>,
    receiver: &Receiver<Work>,
    mode: &AtomicU8,
    in_flight: &AtomicUsize,
    stop: &Stop,
) -> Result<BTreeSet<String>> {
    loop {
        stop.check()?;
        let work = match receiver.try_recv() {
            Ok(work) => work,
            Err(TryRecvError::Empty) => {
                mode.store(early.direct_mode()?, Ordering::Release);
                match receiver.recv_timeout(POLL) {
                    Ok(work) => work,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        bail!("PAC input worker stopped before completion")
                    }
                }
            }
            Err(TryRecvError::Disconnected) => bail!("PAC input worker stopped before completion"),
        };
        mode.store(BUSY, Ordering::Release);
        stop.check()?;
        match work {
            Work::Entry {
                payload,
                future,
                unknown,
            } => {
                let result = early.accept(payload, &future, unknown);
                in_flight.fetch_sub(1, Ordering::AcqRel);
                result?;
            }
            Work::Done(empty) => {
                early.flush(&[], false)?;
                ensure!(
                    early.engine.cached.is_empty(),
                    "ROM mode requires --fda or an FDA image from a PAC"
                );
                early.engine.target.connect()?;
                ensure!(
                    !early.engine.target.is_rom(),
                    "ROM mode requires --fda or an FDA image from a PAC"
                );
                return Ok(empty);
            }
        }
    }
}

pub(super) fn run<T: Target>(
    options: &Cli,
    settings: &Config,
    actions: &Actions,
    directory: &Path,
    target: T,
) -> Result<(T, BTreeSet<String>)> {
    run_with_input(options, settings, actions, directory, target, open_input)
}

fn run_with_input<T: Target>(
    options: &Cli,
    settings: &Config,
    actions: &Actions,
    directory: &Path,
    target: T,
    open: impl FnMut(&Path, &Arc<Stop>) -> Result<Box<dyn Read + Send>> + Send,
) -> Result<(T, BTreeSet<String>)> {
    options.validate_stdin()?;
    let stop = Arc::new(Stop::default());
    let mode = Arc::new(AtomicU8::new(FLAT_READY));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::sync_channel(16);
    let mut early = Early {
        engine: Engine {
            target,
            directory,
            options,
            actions,
            cached: VecDeque::new(),
        },
        committed: BTreeMap::new(),
        images_started: false,
        stop: stop.clone(),
    };
    thread::scope(|scope| {
        // A target panic must also release a producer waiting for more input.
        let mut cancel_on_drop = CancelOnDrop {
            stop: Some(stop.clone()),
            receiver: Some(receiver),
        };
        let producer_stop = stop.clone();
        let producer_mode = mode.clone();
        let producer_in_flight = in_flight.clone();
        let producer = scope.spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                producer_stop.register_input()?;
                produce(
                    options,
                    settings,
                    actions,
                    directory,
                    &sender,
                    &producer_mode,
                    &producer_in_flight,
                    &producer_stop,
                    open,
                )
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("PAC input worker panicked")));
            if result.is_err() {
                producer_stop.cancel(INPUT_FAILED);
            }
            result
        });
        let result = consume(
            &mut early,
            cancel_on_drop.receiver.as_ref().unwrap(),
            &mode,
            &in_flight,
            &stop,
        );
        if result.is_err() {
            stop.cancel(USB_FAILED);
        }
        // Wake a producer blocked on the bounded work queue before joining it.
        drop(cancel_on_drop.receiver.take());
        stop.wait_for_input();
        let produced = producer
            .join()
            .map_err(|_| anyhow::anyhow!("PAC input worker panicked"))?;
        cancel_on_drop.stop = None;
        let empty = if stop.0.load(Ordering::Acquire) == INPUT_FAILED {
            produced?;
            result?
        } else {
            let empty = result?;
            produced?;
            empty
        };
        Ok((early.engine.target, empty))
    })
}

#[cfg(test)]
mod tests;
