use super::super::tests::packet;
use super::*;
use clap::Parser;
use std::{os::unix::net::UnixStream, path::PathBuf, time::Instant};

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
            // Model USB initialization holding the main thread. The input
            // producer must still create and write the later rootfs cache.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if std::fs::read_dir(&self.cache)?
                    .filter_map(|entry| entry.ok())
                    .any(|entry| {
                        entry
                            .metadata()
                            .is_ok_and(|meta| meta.len() >= CHUNK as u64)
                    })
                {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "input caching stopped during FDA initialization"
                );
                thread::sleep(Duration::from_millis(1));
            }
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
                // Waiting for the entire cache to finish would deadlock here.
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
fn fda_runs_early_input_caches_during_boot_and_flash_reads_a_growing_file() {
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
    let (reader, mut writer) = UnixStream::pair().unwrap();
    let prefix = bytes[..sdimage::pac::TABLE_END as usize + 5].to_vec();
    let writer = thread::spawn(move || {
        let _ = writer.write_all(&prefix);
        writer
    });
    let file = File::from(std::os::fd::OwnedFd::from(reader));
    let mut file = Some(file);
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
    let (reader, mut writer) = UnixStream::pair().unwrap();
    let writer = thread::spawn(move || writer.write_all(&bytes));
    let mut file = Some(File::from(std::os::fd::OwnedFd::from(reader)));
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
    let (reader, mut writer) = UnixStream::pair().unwrap();
    let prefix = bytes[..sdimage::pac::TABLE_END as usize + 5].to_vec();
    let writer = thread::spawn(move || {
        let _ = writer.write_all(&prefix);
        writer
    });
    let mut file = Some(File::from(std::os::fd::OwnedFd::from(reader)));
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
fn usb_failure_closes_a_partially_written_cache_and_debug_can_retain_it() {
    let rootfs = vec![0x5a; CHUNK * 4];
    let bytes = packet(&[
        ("FDA", 0, b"ROM"),
        ("agent", 1, b"da"),
        ("rootfs", 6, &rootfs),
    ]);
    let prefix = bytes[..sdimage::pac::TABLE_END as usize + 3 + 2 + CHUNK].to_vec();
    for debug in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let prefix = prefix.clone();
        let writer = thread::spawn(move || {
            let _ = writer.write_all(&prefix);
            writer
        });
        let mut file = Some(File::from(std::os::fd::OwnedFd::from(reader)));
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
