use super::super::tests::packet;
use super::*;
use clap::Parser;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64},
    time::Instant,
};

#[cfg(unix)]
fn input_pipe() -> (File, File) {
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    (
        File::from(std::os::fd::OwnedFd::from(reader)),
        File::from(std::os::fd::OwnedFd::from(writer)),
    )
}

#[test]
fn input_pipe_preserves_binary_bytes_and_reports_eof() {
    let (reader, mut writer) = input_pipe();
    let bytes = vec![0, b'\r', b'\n', 0x1a, 0xff, 0x80];
    let expected = bytes.clone();
    let writer = thread::spawn(move || writer.write_all(&bytes));
    let stop = Arc::new(Stop::default());
    stop.register_input().unwrap();
    let mut input = Input { file: reader, stop };
    let mut actual = vec![];
    input.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, expected);
    writer.join().unwrap().unwrap();
}

#[test]
fn interrupted_cache_write_checks_cancellation_before_retrying() {
    struct InterruptedWriter {
        stop: Arc<Stop>,
        calls: usize,
    }
    impl Write for InterruptedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            self.stop.cancel(USB_FAILED);
            Err(io::ErrorKind::Interrupted.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let stop = Arc::new(Stop::default());
    let mut writer = InterruptedWriter {
        stop: stop.clone(),
        calls: 0,
    };
    assert!(write_cache(&mut writer, b"payload", &stop).is_err());
    assert_eq!(writer.calls, 1);
}

#[test]
fn explicit_fda_stdin_is_binary_and_precedes_pac_bootstrap() {
    for with_pac in [false, true] {
        let input = tempfile::tempdir().unwrap();
        let path = input.path().join("firmware.pac");
        std::fs::write(
            &path,
            packet(&[
                ("FDA", 0, b"ignored"),
                ("da", 2, b"agent"),
                ("kernel", 6, b"image"),
            ]),
        )
        .unwrap();
        let mut options = Cli::try_parse_from(["sddownload", "--fda", "-"]).unwrap();
        if with_pac {
            options.emmc1 = Some(path);
        }
        let directory = tempfile::tempdir().unwrap();
        let peer = Peer {
            rom: true,
            cache: directory.path().into(),
            ..Peer::default()
        };
        let bytes = b"ROM\0\r\n\x1a\xff";
        let (peer, _) = run_with_input(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            peer,
            |path, _| {
                if path == Path::new("-") {
                    Ok(Box::new(io::Cursor::new(bytes.to_vec())))
                } else {
                    Ok(Box::new(File::open(path)?))
                }
            },
        )
        .unwrap();
        assert_eq!(peer.calls[0], ("FDA".into(), bytes.to_vec()));
        assert_eq!(peer.calls.len(), if with_pac { 3 } else { 1 });
        assert!(!peer.rom);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[cfg(windows)]
#[test]
fn windows_cancellation_retries_if_it_arrives_just_before_readfile() {
    let (mut reader, mut writer) = input_pipe();
    let stop = Arc::new(Stop::default());
    let (checked_tx, checked_rx) = mpsc::sync_channel(1);
    let (read_tx, read_rx) = mpsc::sync_channel(1);
    let (rescue_tx, rescue_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    // Bound a broken implementation's wait without terminating a thread.
    let rescue = thread::spawn(move || {
        if rescue_rx.recv().is_ok() && done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            let _ = writer.write_all(b"x");
        }
    });
    thread::scope(|scope| {
        let input_stop = stop.clone();
        let worker = scope.spawn(move || -> io::Result<()> {
            input_stop.register_input()?;
            input_stop.check()?;
            checked_tx.send(()).unwrap();
            read_rx.recv().unwrap();
            // Deliberately bypass Input's pre-read check to reproduce the tiny
            // cancellation-after-check/before-ReadFile window deterministically.
            let _ = reader.read(&mut [0]);
            input_stop.check()
        });
        checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        rescue_tx.send(()).unwrap();
        let start = Instant::now();
        stop.cancel(USB_FAILED); // No I/O is pending yet: ERROR_NOT_FOUND.
        read_tx.send(()).unwrap();
        stop.wait_for_input();
        let result = worker.join().unwrap();
        let elapsed = start.elapsed();
        let _ = done_tx.send(());
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(1),
            "ReadFile cancellation required the rescue writer"
        );
    });
    rescue.join().unwrap();
}

#[cfg(windows)]
fn input_pipe() -> (File, File) {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    let (mut reader, mut writer) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: both output pointers are valid; null attributes create private,
    // non-inheritable synchronous handles. Adopt each fresh handle once.
    assert_ne!(
        unsafe { CreatePipe(&mut reader, &mut writer, std::ptr::null(), 0) },
        0
    );
    unsafe {
        (
            File::from(OwnedHandle::from_raw_handle(reader)),
            File::from(OwnedHandle::from_raw_handle(writer)),
        )
    }
}

#[derive(Default)]
struct Flags {
    fda_started: bool,
    rootfs_started: bool,
}

#[derive(Default)]
struct Gate {
    flags: Mutex<Flags>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self, stop: &Stop, ready: impl Fn(&Flags) -> bool) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut flags = self.flags.lock().unwrap();
        while !ready(&flags) {
            stop.check()?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "pipeline made no progress",
                ));
            }
            flags = self.changed.wait_timeout(flags, POLL).unwrap().0;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Peer {
    rom: bool,
    agent: bool,
    fail_flash: bool,
    panic_flash: bool,
    calls: Vec<(String, Vec<u8>)>,
    gate: Option<Arc<Gate>>,
    cache: PathBuf,
}

impl Peer {
    fn receive(&mut self, name: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
        let mut bytes = vec![];
        reader.read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 == size, "wrong payload boundary");
        self.calls.push((name.into(), bytes));
        Ok(())
    }
}

impl Target for Peer {
    fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_rom(&self) -> bool {
        self.rom
    }
    fn has_agent(&mut self) -> Result<bool> {
        Ok(self.agent)
    }
    fn loader(&mut self, reader: &mut dyn Read, size: u64) -> Result<()> {
        if !self.rom {
            return Ok(());
        }
        self.receive("FDA", reader, size)?;
        if let Some(gate) = &self.gate {
            gate.flags.lock().unwrap().fda_started = true;
            gate.changed.notify_all();
        }
        self.rom = false;
        Ok(())
    }
    fn bootstrap(&mut self, name: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
        self.receive(name, reader, size)?;
        if plan::is_da(name) {
            self.agent = true;
        }
        Ok(())
    }
    fn flash(&mut self, name: &str, reader: &mut dyn Read, size: u64) -> Result<()> {
        assert!(!self.panic_flash, "simulated target panic");
        ensure!(!self.fail_flash, "simulated USB failure");
        if name.ends_with("$rootfs") {
            if let Some(gate) = &self.gate {
                // Only the retained FDA and agent caches are needed here;
                // rootfs must use the memory pipe rather than a growing file.
                assert_eq!(std::fs::read_dir(&self.cache)?.count(), 2);
                gate.flags.lock().unwrap().rootfs_started = true;
                gate.changed.notify_all();
            }
        }
        self.receive(name, reader, size)
    }
}

struct GatedInput {
    bytes: io::Cursor<Vec<u8>>,
    gate: Arc<Gate>,
    stop: Arc<Stop>,
    rootfs_start: u64,
    rootfs_end: u64,
}

impl Read for GatedInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let position = self.bytes.position();
        if position >= self.rootfs_start && position < self.rootfs_end {
            self.gate.wait(&self.stop, |flags| flags.fda_started)?;
            if position >= self.rootfs_start + CHUNK as u64 {
                // Withhold most of the payload until USB starts flashing it.
                // Staging the whole image before flashing would deadlock here.
                self.gate.wait(&self.stop, |flags| flags.rootfs_started)?;
            }
        }
        let boundary = if position < self.rootfs_start {
            self.rootfs_start
        } else if position < self.rootfs_start + CHUNK as u64 {
            self.rootfs_start + CHUNK as u64
        } else {
            u64::MAX
        };
        let count = (boundary - position).min(buffer.len() as u64) as usize;
        self.bytes.read(&mut buffer[..count])
    }
}

#[test]
fn fda_runs_early_and_ready_images_stream_without_payload_cache() {
    let directory = tempfile::tempdir().unwrap();
    let rootfs = vec![0x5a; CHUNK * 4];
    let images = packet(&[("rootfs", 6, &rootfs)]);
    let tail = packet(&[("misc", 6, b"")]);
    let bytes = packet(&[
        ("spl", 3, b"SPL"),
        ("FDA", 0, b"ROM"),
        ("OPSIDA", 1, b"agent"),
        ("images", 7, &images),
        ("tail", 7, &tail),
    ]);
    let rootfs_start = 2 * sdimage::pac::TABLE_END + 3 + 3 + 5;
    let gate = Arc::new(Gate::default());
    let input_gate = gate.clone();
    let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
    let peer = Peer {
        rom: true,
        gate: Some(gate.clone()),
        cache: directory.path().into(),
        ..Peer::default()
    };
    let mut bytes = Some(bytes);
    let (peer, _) = run_with_input(
        &options,
        &Config::load(None).unwrap(),
        &Actions::default(),
        directory.path(),
        peer,
        move |_, stop| {
            Ok(Box::new(GatedInput {
                bytes: io::Cursor::new(bytes.take().unwrap()),
                gate: input_gate.clone(),
                stop: stop.clone(),
                rootfs_start,
                rootfs_end: rootfs_start + (CHUNK * 4) as u64,
            }))
        },
    )
    .unwrap();
    assert_eq!(peer.calls[0], ("FDA".into(), b"ROM".to_vec()));
    assert_eq!(peer.calls[1], ("ospi1$da".into(), b"agent".to_vec()));
    assert_eq!(
        peer.calls
            .iter()
            .find(|(name, _)| name.ends_with("$rootfs"))
            .unwrap()
            .1,
        rootfs
    );
    assert!(gate.flags.lock().unwrap().rootfs_started);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn busy_flash_applies_backpressure_before_reading_the_next_payload() {
    struct GuardedInput {
        bytes: io::Cursor<Vec<u8>>,
        next_payload: u64,
        first_finished: Arc<AtomicBool>,
        position: Arc<AtomicU64>,
        forbidden_read: SyncSender<()>,
    }
    impl Read for GuardedInput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.bytes.position() >= self.next_payload
                && !self.first_finished.load(Ordering::Acquire)
            {
                let _ = self.forbidden_read.try_send(());
                return Err(io::Error::other(
                    "next payload read while current flash was busy",
                ));
            }
            let count = self.bytes.read(buffer)?;
            self.position
                .store(self.bytes.position(), Ordering::Release);
            Ok(count)
        }
    }
    struct SlowPeer {
        calls: Vec<(String, Vec<u8>)>,
        first_finished: Arc<AtomicBool>,
        position: Arc<AtomicU64>,
        next_payload: u64,
        forbidden_read: Receiver<()>,
        cache: PathBuf,
        first_flashes: usize,
        deferred_first: bool,
    }
    impl Target for SlowPeer {
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        fn is_rom(&self) -> bool {
            false
        }
        fn has_agent(&mut self) -> Result<bool> {
            Ok(!self.deferred_first)
        }
        fn loader(&mut self, _: &mut dyn Read, _: u64) -> Result<()> {
            bail!("unexpected FDA")
        }
        fn bootstrap(&mut self, _: &str, _: &mut dyn Read, _: u64) -> Result<()> {
            bail!("unexpected agent")
        }
        fn flash(&mut self, name: &str, source: &mut dyn Read, size: u64) -> Result<()> {
            let needed_cache_files = usize::from(
                self.calls.len() < self.first_flashes
                    && (self.first_flashes == 2 || self.deferred_first),
            );
            assert_eq!(std::fs::read_dir(&self.cache)?.count(), needed_cache_files);
            let mut data = vec![];
            source.read_to_end(&mut data)?;
            assert_eq!(data.len() as u64, size);
            if self.calls.len() + 1 == self.first_flashes {
                // Model slow on-device programming/verification after the last
                // payload byte, before the command's successful response.
                assert!(matches!(
                    self.forbidden_read.recv_timeout(Duration::from_millis(100)),
                    Err(RecvTimeoutError::Timeout)
                ));
                assert_eq!(self.position.load(Ordering::Acquire), self.next_payload);
                assert_eq!(std::fs::read_dir(&self.cache)?.count(), needed_cache_files);
                self.first_finished.store(true, Ordering::Release);
            }
            self.calls.push((name.into(), data));
            Ok(())
        }
    }
    // Cover ordinary streaming, A/B replay, and replaying an older deferred
    // image before the newly handed-off payload is permitted to start reading.
    for (ab, deferred_first) in [(false, false), (true, false), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
        let first = vec![0x11; CHUNK * 2];
        let second = vec![0x22; CHUNK * 8];
        let third = b"last";
        let bytes = if deferred_first {
            let nested = packet(&[("second", 6, &second), ("third", 6, third)]);
            packet(&[("first", 6, &first), ("nested", 7, &nested)])
        } else {
            packet(&[
                (if ab { "first_a" } else { "first" }, 6, &first),
                ("second", 6, &second),
                ("third", 6, third),
            ])
        };
        let next_payload = sdimage::pac::TABLE_END
            + first.len() as u64
            + if deferred_first {
                sdimage::pac::TABLE_END
            } else {
                0
            };
        let first_finished = Arc::new(AtomicBool::new(false));
        let position = Arc::new(AtomicU64::new(0));
        let (forbidden_tx, forbidden_rx) = mpsc::sync_channel(1);
        let peer = SlowPeer {
            calls: vec![],
            first_finished: first_finished.clone(),
            position: position.clone(),
            next_payload,
            forbidden_read: forbidden_rx,
            cache: directory.path().into(),
            first_flashes: if ab { 2 } else { 1 },
            deferred_first,
        };
        let mut input = Some(GuardedInput {
            bytes: io::Cursor::new(bytes),
            next_payload,
            first_finished,
            position,
            forbidden_read: forbidden_tx,
        });
        let mut options = Cli::try_parse_from(["sddownload", "--emmc1", "-"]).unwrap();
        options.b_part = ab;
        let (peer, _) = run_with_input(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            peer,
            |_, _| Ok(Box::new(input.take().unwrap())),
        )
        .unwrap();
        let mut expected = if ab {
            vec![
                ("emmc1$$first_a".into(), first.clone()),
                ("emmc1$$first_b".into(), first),
            ]
        } else {
            vec![("emmc1$$first".into(), first)]
        };
        expected.extend(if deferred_first {
            [
                ("emmc1$nested$second".into(), second),
                ("emmc1$nested$third".into(), third.to_vec()),
            ]
        } else {
            [
                ("emmc1$$second".into(), second),
                ("emmc1$$third".into(), third.to_vec()),
            ]
        });
        assert_eq!(peer.calls, expected);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[test]
fn deferred_and_direct_images_preserve_physical_pac_order_after_bootstrap() {
    let input = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = input.path().join("ordered.pac");
    let nested = packet(&[("second", 6, b"B"), ("third", 6, b"C")]);
    let mut bytes = packet(&[
        ("first", 6, b"A"),
        ("FDA", 0, b"ROM"),
        ("OPSIDA", 1, b"agent"),
        ("nested", 7, &nested),
    ]);
    // Header row order need not equal physical payload order. Forward traversal
    // follows offsets; bootstrap is the only required reordering in this PAC.
    for index in 0..128 {
        bytes.swap(1024 + index, 1024 + 3 * 128 + index);
    }
    let mut crc = sdimage::BZIP2.digest();
    crc.update(&bytes[..1020]);
    crc.update(&bytes[1024..]);
    bytes[1020..1024].copy_from_slice(&crc.finalize().to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    let options = Cli::try_parse_from(["sddownload", "--emmc1", path.to_str().unwrap()]).unwrap();
    let (peer, _) = run(
        &options,
        &Config::load(None).unwrap(),
        &Actions::default(),
        directory.path(),
        Peer {
            rom: true,
            ..Peer::default()
        },
    )
    .unwrap();
    assert_eq!(
        peer.calls,
        [
            ("FDA".into(), b"ROM".to_vec()),
            ("ospi1$da".into(), b"agent".to_vec()),
            ("emmc1$$first".into(), b"A".to_vec()),
            ("emmc1$nested$second".into(), b"B".to_vec()),
            ("emmc1$nested$third".into(), b"C".to_vec())
        ]
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn identical_late_fda_is_ignored_but_conflicting_fda_requests_strict_mode() {
    let input = tempfile::tempdir().unwrap();
    let path = input.path().join("loaders.pac");
    for replacement in [b"ROM".as_slice(), b"NEW".as_slice()] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            &path,
            packet(&[
                ("first-fda", 0, b"ROM"),
                ("agent", 1, b"da"),
                ("kernel", 6, b"image"),
                ("last-fda", 0, replacement),
            ]),
        )
        .unwrap();
        let options =
            Cli::try_parse_from(["sddownload", "--emmc1", path.to_str().unwrap()]).unwrap();
        let peer = Peer {
            rom: true,
            ..Peer::default()
        };
        let result = run(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            peer,
        );
        if replacement == b"ROM" {
            let (peer, _) = result.unwrap();
            assert_eq!(
                peer.calls.iter().filter(|(name, _)| name == "FDA").count(),
                1
            );
        } else {
            let error = result.err().unwrap().to_string();
            assert!(error.contains("conflicting late FDA") && error.contains("--verify-first"));
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let mut strict = options;
        strict.verify_first = true;
        let (peer, _) = super::super::stream_inputs(
            &strict,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                rom: true,
                ..Peer::default()
            },
        )
        .unwrap();
        assert_eq!(peer.calls[0], ("FDA".into(), replacement.to_vec()));
    }
}

#[test]
fn later_inputs_can_add_forward_bootstrap_stages_without_replacing_the_fda() {
    let input = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let ospi = input.path().join("ospi.pac");
    let emmc = input.path().join("emmc.pac");
    std::fs::write(
        &ospi,
        packet(&[
            ("FDA", 0, b"ROM"),
            ("agent", 1, b"ospi"),
            ("spl", 3, b"spl"),
        ]),
    )
    .unwrap();
    std::fs::write(
        &emmc,
        packet(&[
            ("FDA", 0, b"ROM"),
            ("agent", 2, b"emmc"),
            ("kernel", 6, b"kernel"),
        ]),
    )
    .unwrap();
    let options = Cli::try_parse_from([
        "sddownload",
        "--ospi1",
        ospi.to_str().unwrap(),
        "--emmc1",
        emmc.to_str().unwrap(),
    ])
    .unwrap();
    let (peer, _) = run(
        &options,
        &Config::load(None).unwrap(),
        &Actions::default(),
        directory.path(),
        Peer {
            rom: true,
            ..Peer::default()
        },
    )
    .unwrap();
    let names: Vec<_> = peer.calls.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["FDA", "ospi1$da", "ospi1$$spl", "emmc1$da", "emmc1$$kernel"]
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn usb_failure_cancels_input_while_the_pipe_remains_open() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = packet(&[("kernel", 6, b"first"), ("rootfs", 6, b"later")]);
    let (reader, mut writer) = input_pipe();
    let prefix = bytes[..sdimage::pac::TABLE_END as usize + 5].to_vec();
    let writer = thread::spawn(move || {
        let _ = writer.write_all(&prefix);
        writer
    });
    let mut file = Some(reader);
    let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
    let start = Instant::now();
    let error = run_with_input(
        &options,
        &Config::load(None).unwrap(),
        &Actions::default(),
        directory.path(),
        Peer {
            fail_flash: true,
            ..Peer::default()
        },
        move |_, stop| {
            Ok(Box::new(Input {
                file: file.take().unwrap(),
                stop: stop.clone(),
            }))
        },
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("simulated USB failure"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    drop(writer.join().unwrap());
}

#[test]
fn truncated_direct_input_reports_the_input_error_and_cleans_cache() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = packet(&[("kernel", 6, b"image")]);
    bytes.pop();
    let (reader, mut writer) = input_pipe();
    let writer = thread::spawn(move || writer.write_all(&bytes));
    let mut file = Some(reader);
    let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
    let error = run_with_input(
        &options,
        &Config::load(None).unwrap(),
        &Actions::default(),
        directory.path(),
        Peer::default(),
        move |_, stop| {
            Ok(Box::new(Input {
                file: file.take().unwrap(),
                stop: stop.clone(),
            }))
        },
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("truncated PAC payload"),
        "{error:#}"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    writer.join().unwrap().unwrap();
}

#[test]
fn target_panic_also_cancels_a_blocked_input_worker() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = packet(&[("kernel", 6, b"first"), ("rootfs", 6, b"later")]);
    let (reader, mut writer) = input_pipe();
    let prefix = bytes[..sdimage::pac::TABLE_END as usize + 5].to_vec();
    let writer = thread::spawn(move || {
        let _ = writer.write_all(&prefix);
        writer
    });
    let mut file = Some(reader);
    let options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
    let start = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_with_input(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            Peer {
                panic_flash: true,
                ..Peer::default()
            },
            move |_, stop| {
                Ok(Box::new(Input {
                    file: file.take().unwrap(),
                    stop: stop.clone(),
                }))
            },
        )
    }));
    assert!(result.is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    drop(writer.join().unwrap());
}

#[test]
fn usb_failure_cleans_bootstrap_caches_and_debug_can_retain_them() {
    let rootfs = vec![0x5a; CHUNK * 4];
    let bytes = packet(&[
        ("FDA", 0, b"ROM"),
        ("agent", 1, b"da"),
        ("rootfs", 6, &rootfs),
    ]);
    let prefix = bytes[..sdimage::pac::TABLE_END as usize + 3 + 2 + CHUNK].to_vec();
    for debug in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (reader, mut writer) = input_pipe();
        let prefix = prefix.clone();
        let writer = thread::spawn(move || {
            let _ = writer.write_all(&prefix);
            writer
        });
        let mut file = Some(reader);
        let mut options = Cli::try_parse_from(["sddownload", "--emmc1", "unused"]).unwrap();
        options.debug = debug;
        let peer = Peer {
            rom: true,
            fail_flash: true,
            gate: Some(Arc::new(Gate::default())),
            cache: directory.path().into(),
            ..Peer::default()
        };
        let error = run_with_input(
            &options,
            &Config::load(None).unwrap(),
            &Actions::default(),
            directory.path(),
            peer,
            move |_, stop| {
                Ok(Box::new(Input {
                    file: file.take().unwrap(),
                    stop: stop.clone(),
                }))
            },
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("simulated USB failure"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count() > 0,
            debug
        );
        drop(writer.join().unwrap());
    }
}
