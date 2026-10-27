//! USB matching deliberately uses raw iConfiguration (0=ROM, 4=fastboot),
//! not bConfigurationValue. All protocol I/O is injectable for peer tests.
use anyhow::{bail, ensure, Context as _, Result};
use rusb::{Context, Device, DeviceHandle, Direction, TransferType, UsbContext};
use std::{
    io::Read,
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub trait BulkTransport {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize>;
    fn write(&mut self, buffer: &[u8]) -> Result<usize>;
}

pub fn write_all(transport: &mut impl BulkTransport, bytes: &[u8]) -> Result<()> {
    write_all_with_progress(transport, bytes, |_| {})
}

pub(crate) fn write_all_with_progress(
    transport: &mut impl BulkTransport,
    mut bytes: &[u8],
    mut progress: impl FnMut(u64),
) -> Result<()> {
    while !bytes.is_empty() {
        let n = transport.write(&bytes[..bytes.len().min(0x4000)])?;

        ensure!(
            n > 0 && n <= bytes.len().min(0x4000),
            "USB write made invalid progress"
        );
        bytes = &bytes[n..];
        progress(n as u64);
    }

    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Rom,
    Fastboot,
}

impl Mode {
    pub fn from_configuration_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::Rom),
            4 => Some(Self::Fastboot),
            _ => None,
        }
    }
}

pub struct DeviceInfo {
    pub device: Device<Context>,
    pub configuration: u8,
    pub interface: u8,
    pub alternate: u8,
    pub input: u8,
    pub output: u8,
    pub configuration_index: u8,
    pub serial: String,
}

pub fn discover(protocol: u8, serial: &str) -> Result<Vec<DeviceInfo>> {
    let context = Context::new().context("initialize libusb")?;
    let mut matches = vec![];
    let mut failures = vec![];

    for device in context.devices()?.iter() {
        let descriptor = match device.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        'configs: for i in 0..descriptor.num_configurations() {
            let config = match device.config_descriptor(i) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for interface in config.interfaces() {
                for alt in interface.descriptors() {
                    if (alt.class_code(), alt.sub_class_code(), alt.protocol_code())
                        != (0xff, 0x42, protocol)
                    {
                        continue;
                    }

                    let mut input = None;
                    let mut output = None;

                    for endpoint in alt.endpoint_descriptors() {
                        if endpoint.transfer_type() == TransferType::Bulk {
                            match endpoint.direction() {
                                Direction::In => input = Some(endpoint.address()),
                                Direction::Out => output = Some(endpoint.address()),
                            }
                        }
                    }

                    let (Some(input), Some(output)) = (input, output) else {
                        continue;
                    };
                    let handle = match device.open() {
                        Ok(h) => h,
                        Err(e) => {
                            failures.push(format!(
                                "USB {:04x}:{:04x}: {e} (check udev/device permissions)",
                                descriptor.vendor_id(),
                                descriptor.product_id()
                            ));
                            break 'configs;
                        }
                    };
                    let found_serial = if descriptor.serial_number_string_index().is_some() {
                        match handle.read_serial_number_string_ascii(&descriptor) {
                            Ok(s) => s,
                            Err(e) => {
                                failures.push(format!("read USB serial: {e}"));
                                break 'configs;
                            }
                        }
                    } else {
                        String::new()
                    };

                    if !serial.is_empty() && found_serial != serial {
                        continue;
                    }
                    matches.push(DeviceInfo {
                        device: device.clone(),
                        configuration: config.number(),
                        interface: alt.interface_number(),
                        alternate: alt.setting_number(),
                        input,
                        output,
                        configuration_index: config.description_string_index().unwrap_or(0),
                        serial: found_serial,
                    });
                    break 'configs;
                }
            }
        }
    }

    if matches.is_empty() && !failures.is_empty() {
        bail!("{}", failures.join("; "))
    }

    Ok(matches)
}

pub fn wait_for(protocol: u8, serial: &str, timeout: Duration) -> Result<DeviceInfo> {
    let start = Instant::now();

    loop {
        let mut matches = discover(protocol, serial)?;

        if protocol == 3 {
            matches.retain(|d| Mode::from_configuration_index(d.configuration_index).is_some());
        }

        ensure!(
            matches.len() <= 1,
            "multiple compatible USB devices; select one with --sn"
        );

        if let Some(info) = matches.pop() {
            return Ok(info);
        }

        ensure!(
            timeout.is_zero() || start.elapsed() < timeout,
            "timed out waiting for USB device"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn wait_for_mode(serial: &str, timeout: Duration, mode: Mode) -> Result<DeviceInfo> {
    let start = Instant::now();

    loop {
        let mut matches = discover(3, serial)?;
        matches.retain(|d| Mode::from_configuration_index(d.configuration_index) == Some(mode));

        ensure!(
            matches.len() <= 1,
            "multiple compatible USB devices; select one with --sn"
        );

        if let Some(info) = matches.pop() {
            return Ok(info);
        }

        ensure!(
            timeout.is_zero() || start.elapsed() < timeout,
            "timed out waiting for {mode:?} device"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The timeout limits silence between successful USB transfers, not the total
/// duration of a connection, command, or image. Zero keeps libusb's unlimited
/// timeout convention. Explicit clock values keep the policy testable without
/// hardware or sleeps.
struct IdleTimeout {
    timeout: Duration,
    deadline: Option<Instant>,
}

impl IdleTimeout {
    fn new(timeout: Duration, now: Instant) -> Result<Self> {
        let mut idle = Self {
            timeout,
            deadline: None,
        };
        idle.reset(now)?;
        Ok(idle)
    }

    fn reset(&mut self, now: Instant) -> Result<()> {
        self.deadline = if self.timeout.is_zero() {
            None
        } else {
            Some(
                now.checked_add(self.timeout)
                    .context("USB timeout exceeds clock range")?,
            )
        };
        Ok(())
    }

    fn remaining(&self, now: Instant) -> Result<Duration> {
        match self.deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(now);
                if remaining.is_zero() {
                    return Err(anyhow::Error::new(rusb::Error::Timeout).context(format!(
                        "USB transfer inactivity timeout ({:?} without progress)",
                        self.timeout
                    )));
                }
                Ok(remaining)
            }
            None => Ok(Duration::ZERO),
        }
    }

    fn transfer(
        &mut self,
        mut clock: impl FnMut() -> Instant,
        operation: impl FnOnce(Duration) -> rusb::Result<usize>,
    ) -> Result<usize> {
        let count = operation(self.remaining(clock())?)?;
        if count > 0 {
            self.reset(clock())?;
        }
        Ok(count)
    }
}

pub struct UsbLink {
    handle: DeviceHandle<Context>,
    interface: u8,
    input: u8,
    output: u8,
    idle: IdleTimeout,
}

impl UsbLink {
    pub fn open(info: DeviceInfo, timeout: Duration) -> Result<Self> {
        let handle = info.device.open()?;

        if handle.active_configuration().ok() != Some(info.configuration) {
            handle.set_active_configuration(info.configuration)?;
        }

        match handle.set_auto_detach_kernel_driver(true) {
            Ok(()) | Err(rusb::Error::NotSupported) => {}

            Err(e) => return Err(e.into()),
        }
        handle
            .claim_interface(info.interface)
            .context("claim USB interface (another downloader/ADB server may own it)")?;

        if info.alternate != 0 {
            handle.set_alternate_setting(info.interface, info.alternate)?;
        }

        Ok(Self {
            handle,
            interface: info.interface,
            input: info.input,
            output: info.output,
            idle: IdleTimeout::new(timeout, Instant::now())?,
        })
    }

    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.idle = IdleTimeout::new(timeout, Instant::now())?;
        Ok(())
    }
}

impl BulkTransport for UsbLink {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let n = buffer.len().min(0x4000);

        if n == 0 {
            return Ok(0);
        }

        let count = self
            .idle
            .transfer(Instant::now, |timeout| {
                self.handle.read_bulk(self.input, &mut buffer[..n], timeout)
            })
            .with_context(|| format!("USB bulk read (inactivity limit {:?})", self.idle.timeout))?;

        ensure!(count > 0, "USB disconnected/empty read");

        Ok(count)
    }

    fn write(&mut self, buffer: &[u8]) -> Result<usize> {
        let n = buffer.len().min(0x4000);
        if n == 0 {
            return Ok(0);
        }
        let count = self
            .idle
            .transfer(Instant::now, |timeout| {
                self.handle.write_bulk(self.output, &buffer[..n], timeout)
            })
            .with_context(|| {
                format!("USB bulk write (inactivity limit {:?})", self.idle.timeout)
            })?;
        ensure!(count > 0, "USB bulk write made no progress");
        Ok(count)
    }
}

impl Drop for UsbLink {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(self.interface);
    }
}

/// SemiDrive ROM easyboot is a raw file stream; it is not a subprocess or
/// a fastboot download command. This client works with any bulk transport.
pub struct EasybootClient<T: BulkTransport> {
    pub transport: T,
}

impl<T: BulkTransport> EasybootClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn download(&mut self, path: impl AsRef<Path>) -> Result<u64> {
        let mut file = std::fs::File::open(path)?;

        let size = file.metadata()?.len();

        self.download_reader(&mut file, size)
    }

    pub fn download_reader<R: Read + ?Sized>(&mut self, source: &mut R, size: u64) -> Result<u64> {
        ensure!(size > 0, "ROM loader is empty");

        let mut buffer = vec![0; 0x4000];
        let mut remaining = size;

        while remaining != 0 {
            let count = remaining.min(buffer.len() as u64) as usize;

            source
                .read_exact(&mut buffer[..count])
                .context("ROM loader was truncated")?;
            write_all(&mut self.transport, &buffer[..count])?;

            remaining -= count as u64;
        }

        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn active_full_partial_and_response_transfers_can_exceed_total_timeout() {
        let start = Instant::now();
        let now = Cell::new(start);
        let limit = Duration::from_secs(400);
        let mut idle = IdleTimeout::new(limit, start).unwrap();
        for index in 0..1000 {
            let count = [0x4000, 1, 64][index % 3];
            assert_eq!(
                idle.transfer(
                    || now.get(),
                    |timeout| {
                        assert_eq!(timeout, limit);
                        now.set(now.get() + Duration::from_secs(1));
                        Ok(count)
                    }
                )
                .unwrap(),
                count
            );
        }
        assert!(now.get().duration_since(start) > limit);
        assert_eq!(idle.remaining(now.get()).unwrap(), limit);
    }

    #[test]
    fn stalled_transfer_uses_remaining_idle_budget_and_does_not_renew_it() {
        let start = Instant::now();
        let now = Cell::new(start + Duration::from_secs(2));
        let mut idle = IdleTimeout::new(Duration::from_secs(5), start).unwrap();
        let error = idle
            .transfer(
                || now.get(),
                |timeout| {
                    assert_eq!(timeout, Duration::from_secs(3));
                    now.set(start + Duration::from_secs(5));
                    Err(rusb::Error::Timeout)
                },
            )
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<rusb::Error>(),
            Some(&rusb::Error::Timeout)
        );
        let called = Cell::new(false);
        let error = idle
            .transfer(
                || now.get(),
                |_| {
                    called.set(true);
                    Ok(1)
                },
            )
            .unwrap_err();
        assert!(!called.get());
        assert!(error.to_string().contains("inactivity timeout"));
        assert_eq!(
            error.downcast_ref::<rusb::Error>(),
            Some(&rusb::Error::Timeout)
        );
    }

    #[test]
    fn zero_length_result_does_not_count_as_transfer_progress() {
        let start = Instant::now();
        let mut idle = IdleTimeout::new(Duration::from_secs(5), start).unwrap();
        let almost_expired = start + Duration::from_secs(4);
        assert_eq!(idle.transfer(|| almost_expired, |_| Ok(0)).unwrap(), 0);
        assert_eq!(
            idle.remaining(almost_expired).unwrap(),
            Duration::from_secs(1)
        );
        assert!(idle.remaining(start + Duration::from_secs(5)).is_err());
    }

    #[test]
    fn progress_renews_timeout_from_transfer_completion() {
        let start = Instant::now();
        let now = Cell::new(start + Duration::from_secs(4));
        let mut idle = IdleTimeout::new(Duration::from_secs(5), start).unwrap();
        idle.transfer(
            || now.get(),
            |timeout| {
                assert_eq!(timeout, Duration::from_secs(1));
                now.set(start + Duration::from_secs(5));
                Ok(1)
            },
        )
        .unwrap();
        assert_eq!(idle.remaining(now.get()).unwrap(), Duration::from_secs(5));
        assert_eq!(
            idle.remaining(start + Duration::from_secs(9)).unwrap(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn zero_timeout_remains_unlimited_and_unrepresentable_values_are_rejected() {
        let start = Instant::now();
        let later = start + Duration::from_secs(1_000_000);
        let mut idle = IdleTimeout::new(Duration::ZERO, start).unwrap();
        idle.transfer(
            || later,
            |timeout| {
                assert_eq!(timeout, Duration::ZERO);
                Ok(64)
            },
        )
        .unwrap();
        assert_eq!(idle.remaining(later).unwrap(), Duration::ZERO);
        assert!(IdleTimeout::new(Duration::MAX, start).is_err());
    }

    struct Peer(Vec<u8>);
    impl BulkTransport for Peer {
        fn read(&mut self, _: &mut [u8]) -> Result<usize> {
            bail!("unexpected ROM read")
        }

        fn write(&mut self, b: &[u8]) -> Result<usize> {
            let n = b.len().min(37);
            self.0.extend_from_slice(&b[..n]);

            Ok(n)
        }
    }

    #[test]
    fn rom_is_unframed_and_handles_partial_writes() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let data = vec![0x42; 40000];
        std::fs::write(f.path(), &data).unwrap();
        let mut client = EasybootClient::new(Peer(vec![]));
        assert_eq!(client.download(f.path()).unwrap(), 40000);
        assert_eq!(client.transport.0, data);
    }

    #[test]
    fn modes_use_string_index() {
        assert_eq!(Mode::from_configuration_index(0), Some(Mode::Rom));
        assert_eq!(Mode::from_configuration_index(4), Some(Mode::Fastboot));
        assert_eq!(Mode::from_configuration_index(1), None);
    }
}
