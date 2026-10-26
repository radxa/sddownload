//! Standard command/response types come from fastboot-protocol; SemiDrive's
//! MD5/read/flash-seq extensions are layered over its standard codec.
use crate::usb::{write_all, write_all_with_progress, BulkTransport};
use anyhow::{bail, ensure, Context, Result};
use fastboot_protocol::protocol::{FastBootCommand, FastBootResponse};
use md5::{Digest, Md5};
use sdimage::sparse::StreamImage;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, Write},
    path::Path,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    Okay(String),
    Data(u64),
    Md5(String),
}

/// Flash transfer progress. Sparse holes count toward `completed` and `total`,
/// but only payload bytes successfully written to USB count as `transferred`.
/// Reaching `total` does not imply that the final device response has succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlashProgress {
    pub completed: u64,
    pub total: u64,
    pub transferred: u64,
}

/// Names are preserved exactly as advertised by the device, including storage
/// prefixes. Missing fields mean that the server did not provide usable data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionInfo {
    pub name: String,
    pub size: Option<u64>,
    pub kind: Option<String>,
    pub logical: Option<bool>,
}

#[derive(Debug)]
pub struct PartitionListing {
    pub partitions: Vec<PartitionInfo>,
    pub warning: Option<String>,
}

fn advertised_partitions(lines: impl IntoIterator<Item = String>) -> Vec<PartitionInfo> {
    let mut partitions: BTreeMap<String, PartitionInfo> = BTreeMap::new();
    for line in lines {
        for (prefix, field) in [
            ("partition-size:", 0),
            ("partition-type:", 1),
            ("is-logical:", 2),
        ] {
            let Some(value) = line.trim().strip_prefix(prefix) else {
                continue;
            };
            let Some((name, value)) = value.rsplit_once(':') else {
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            if name.is_empty() || !name.is_ascii() || name.chars().any(char::is_control) {
                continue;
            }
            let partition = partitions
                .entry(name.into())
                .or_insert_with(|| PartitionInfo {
                    name: name.into(),
                    ..PartitionInfo::default()
                });
            match field {
                // Fastboot partition-size values use hexadecimal, with an
                // optional 0x prefix, rather than PAC payload byte counts.
                0 => {
                    partition.size = u64::from_str_radix(
                        value
                            .strip_prefix("0x")
                            .or_else(|| value.strip_prefix("0X"))
                            .unwrap_or(value),
                        16,
                    )
                    .ok()
                }
                1 if !value.is_empty() => partition.kind = Some(value.into()),
                2 => {
                    partition.logical = match value {
                        "yes" => Some(true),
                        "no" => Some(false),
                        _ => None,
                    }
                }
                _ => {}
            }
        }
    }
    partitions.into_values().collect()
}

#[derive(Debug)]
pub struct DeviceFailure(pub String);

impl std::fmt::Display for DeviceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "fastboot FAIL: {}", self.0)
    }
}

impl std::error::Error for DeviceFailure {}

pub struct FastbootClient<T: BulkTransport> {
    pub transport: T,
    pub information: Vec<String>,
}

pub fn partition_name(name: &str) -> String {
    let name = name.replace("\\$", "$");

    match (name.find("$$"), name.find("-0x")) {
        (Some(start), Some(end)) if end > start => {
            format!("{}{}", &name[..start + 2], &name[end + 1..])
        }
        _ => name,
    }
}

impl<T: BulkTransport> FastbootClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            information: vec![],
        }
    }

    pub fn response(&mut self) -> Result<Response> {
        loop {
            let mut packet = [0; 64];
            let n = self.transport.read(&mut packet)?;

            ensure!(n <= 64, "oversized fastboot response");
            let bytes = &packet[..n];

            // SemiDrive readback can announce lengths beyond standard fastboot's
            // 32-bit DATA field. Ordinary replies still use the upstream codec.
            if let Some(payload) = bytes.strip_prefix(b"DATA") {
                let value = std::str::from_utf8(payload)?.trim_end_matches('\0');

                if value.len() > 8 {
                    return Ok(Response::Data(u64::from_str_radix(value, 16)?));
                }
            }

            if bytes.starts_with(b"MD5") {
                let value = std::str::from_utf8(&bytes[3..])?.trim_end_matches('\0');

                ensure!(
                    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid MD5 response"
                );

                return Ok(Response::Md5(value.into()));
            }

            match FastBootResponse::from_bytes(bytes).context("invalid fastboot response")? {
                FastBootResponse::Okay(s) => return Ok(Response::Okay(s)),
                FastBootResponse::Data(n) => return Ok(Response::Data(n.into())),
                FastBootResponse::Fail(s) => return Err(DeviceFailure(s).into()),
                FastBootResponse::Info(s) | FastBootResponse::Text(s) => {
                    eprintln!("{s}");
                    self.information.push(s);
                }
            }
        }
    }

    pub fn send(&mut self, command: &str) -> Result<()> {
        ensure!(
            command.is_ascii() && command.len() <= 64 && !command.contains('\0'),
            "fastboot commands must be at most 64 non-NUL ASCII bytes"
        );
        self.information.clear();
        write_all(&mut self.transport, command.as_bytes())
    }

    pub fn command(&mut self, command: &str) -> Result<String> {
        self.send(command)?;

        match self.response()? {
            Response::Okay(s) => Ok(s),
            other => bail!("expected OKAY, received {other:?}"),
        }
    }

    pub fn getvar(&mut self, name: &str) -> Result<String> {
        let value = self.command(&FastBootCommand::GetVar(name).to_string())?;

        if value.is_empty() {
            Ok(self.information.join("\n"))
        } else {
            Ok(value)
        }
    }

    /// Best-effort enumeration through getvar:all. Partition enumeration is
    /// optional; individual partition-size queries require names already known.
    /// Never issues OEM, flash, erase, reboot, or partition readback commands.
    pub fn list_partitions(&mut self) -> Result<PartitionListing> {
        let result = self.getvar("all");
        let mut lines = self.information.clone();
        let warning = match result {
            Ok(value) => {
                lines.extend(value.lines().map(str::to_owned));
                None
            }
            Err(error) if error.downcast_ref::<DeviceFailure>().is_some() => {
                Some(error.to_string())
            }
            Err(error) => return Err(error.context("query partition information with getvar:all")),
        };
        let partitions = advertised_partitions(lines);
        ensure!(!partitions.is_empty(),
            "fastboot did not advertise any partitions through getvar:all{}; this firmware may not support enumeration; use --list-partitions with a PAC to inspect its image entries",
            warning.as_ref().map(|error| format!(" ({error})")).unwrap_or_default());
        Ok(PartitionListing {
            partitions,
            warning,
        })
    }

    pub fn download(&mut self, source: &mut impl Read, size: u64, verify: bool) -> Result<()> {
        self.download_with_progress(source, size, verify, |_| {})
    }

    fn download_with_progress(
        &mut self,
        source: &mut impl Read,
        size: u64,
        verify: bool,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        ensure!(
            size > 0 && size <= u32::MAX as u64,
            "invalid fastboot download size"
        );
        self.send(&FastBootCommand::<&str>::Download(size as u32).to_string())?;

        ensure!(
            self.response()? == Response::Data(size),
            "device accepted a different download length"
        );
        let mut left = size;
        let mut digest = Md5::new();
        let mut buffer = vec![0; 1024 * 1024];

        while left > 0 {
            let n = left.min(buffer.len() as u64) as usize;
            source
                .read_exact(&mut buffer[..n])
                .context("truncated image")?;
            write_all_with_progress(&mut self.transport, &buffer[..n], &mut progress)?;
            digest.update(&buffer[..n]);
            left -= n as u64;
        }

        ensure!(
            matches!(self.response()?, Response::Okay(_)),
            "download did not finish with OKAY"
        );

        if verify {
            self.command(&format!("md5:{:x}", digest.finalize()))?;
        }

        Ok(())
    }

    pub fn flash(&mut self, partition: &str, path: &Path, verify: bool) -> Result<()> {
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        self.flash_reader(partition, &mut file, size, verify)
    }

    pub fn flash_reader<R: Read + ?Sized>(
        &mut self,
        partition: &str,
        source: &mut R,
        encoded_size: u64,
        verify: bool,
    ) -> Result<()> {
        self.flash_reader_with_progress(partition, source, encoded_size, verify, |_| {})
    }

    /// Flash a forward-only image, reporting logical image position and actual
    /// USB payload bytes. Callers should mark success only after this returns.
    pub fn flash_reader_with_progress<R: Read + ?Sized>(
        &mut self,
        partition: &str,
        source: &mut R,
        encoded_size: u64,
        verify: bool,
        mut progress: impl FnMut(FlashProgress),
    ) -> Result<()> {
        let partition = partition_name(partition);
        let mut image = StreamImage::new(source, encoded_size)?;

        ensure!(image.size > 0, "cannot flash an empty image");
        let mut state = FlashProgress {
            completed: 0,
            total: image.size,
            transferred: 0,
        };
        progress(state);
        let limit = match self.getvar("max-download-size") {
            Ok(s) => {
                let s = s.rsplit(':').next().unwrap_or(&s).trim();
                let n = if let Some(s) = s.strip_prefix("0x") {
                    u64::from_str_radix(s, 16)?
                } else {
                    s.parse()?
                };

                ensure!(n > 0, "device max-download-size is zero");
                n.min(0x40000000)
            }

            Err(error) if error.downcast_ref::<DeviceFailure>().is_some() => {
                eprintln!("max-download-size unavailable ({error}); using 64 MiB");
                64 * 1024 * 1024
            }

            Err(error) => return Err(error.context("query maximum download size")),
        };

        if !image.sparse && image.size <= limit {
            let size = image.size;
            image.next_segment()?;
            self.download_with_progress(&mut image, size, verify, |count| {
                state.completed += count;
                state.transferred += count;
                progress(state);
            })?;
            image.finish()?;
            self.command(&FastBootCommand::Flash(&partition).to_string())?;

            return Ok(());
        }

        let mut transferred = false;
        while let Some((start, size)) = image.next_segment()? {
            transferred = true;
            state.completed = start;
            progress(state);
            let mut offset = start;
            let mut left = size;

            while left > 0 {
                let count = left.min(limit);

                self.download_with_progress(&mut image, count, verify, |count| {
                    state.completed += count;
                    state.transferred += count;
                    progress(state);
                })?;
                self.send(&format!(
                    "flash-seq:{partition}:off{offset:x}/{:x}",
                    image.size
                ))?;

                // Interior holes advance offsets; a trailing hole is included
                // in the final transfer so the target can identify completion.
                // Intermediate segments deliberately have no response read.
                if offset + count == image.size {
                    ensure!(
                        matches!(self.response()?, Response::Okay(_)),
                        "last flash segment failed"
                    );
                }

                offset += count;
                left -= count;
            }
        }

        image.finish()?;
        ensure!(transferred, "image contains no transferable data");

        Ok(())
    }

    pub fn erase(&mut self, partition: &str) -> Result<()> {
        self.command(&FastBootCommand::Erase(partition_name(partition)).to_string())?;

        Ok(())
    }

    pub fn reboot(&mut self) -> Result<()> {
        self.command(&FastBootCommand::<&str>::Reboot.to_string())?;

        Ok(())
    }

    pub fn readback(&mut self, partition: &str, destination: &Path) -> Result<u64> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));

        ensure!(parent.is_dir(), "readback output parent does not exist");
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let count = self.receive_readback(partition, temp.as_file_mut())?;
        temp.persist(destination)
            .map_err(|e| anyhow::anyhow!("publish readback: {}", e.error))?;
        Ok(count)
    }

    /// Receive and verify a partition before writing anything to the output.
    /// Uses temporary disk storage so stdout/pipes never receive corrupt data.
    pub fn readback_to(&mut self, partition: &str, output: &mut impl Write) -> Result<u64> {
        let mut temp = tempfile::tempfile()?;
        let count = self.receive_readback(partition, &mut temp)?;
        temp.rewind()?;
        ensure!(
            std::io::copy(&mut temp, output)? == count,
            "truncated verified readback"
        );
        output.flush()?;
        Ok(count)
    }

    fn receive_readback(&mut self, partition: &str, output: &mut impl Write) -> Result<u64> {
        self.send(&format!("read:{}:0:0", partition_name(partition)))?;
        let Response::Data(count) = self.response()? else {
            bail!("readback missing DATA response")
        };
        let mut remaining = count;
        let mut digest = Md5::new();
        let mut buffer = vec![0; 0x4000];

        while remaining > 0 {
            let want = remaining.min(buffer.len() as u64) as usize;
            let n = self.transport.read(&mut buffer[..want])?;

            ensure!(n > 0 && n <= want, "invalid readback length");
            output.write_all(&buffer[..n])?;
            digest.update(&buffer[..n]);
            remaining -= n as u64;
        }

        let Response::Md5(expected) = self.response()? else {
            bail!("readback missing MD5 response")
        };

        ensure!(
            expected.eq_ignore_ascii_case(&format!("{:x}", digest.finalize())),
            "readback MD5 mismatch"
        );

        ensure!(
            matches!(self.response()?, Response::Okay(_)),
            "readback missing final OKAY"
        );
        output.flush()?;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Peer {
        responses: VecDeque<Vec<u8>>,
        writes: Vec<Vec<u8>>,
    }

    impl BulkTransport for Peer {
        fn read(&mut self, b: &mut [u8]) -> Result<usize> {
            let p = self.responses.pop_front().context("no queued response")?;

            ensure!(p.len() <= b.len(), "peer response too big");
            b[..p.len()].copy_from_slice(&p);

            Ok(p.len())
        }

        fn write(&mut self, b: &[u8]) -> Result<usize> {
            self.writes.push(b.to_vec());

            Ok(b.len())
        }
    }

    fn peer(responses: &[&[u8]]) -> Peer {
        Peer {
            responses: responses.iter().map(|p| p.to_vec()).collect(),
            writes: vec![],
        }
    }

    #[test]
    fn download_md5() {
        let mut client = FastbootClient::new(peer(&[b"DATA00000003", b"OKAY", b"OKAY"]));
        client.download(&mut &b"abc"[..], 3, true).unwrap();
        assert_eq!(
            client.transport.writes,
            [
                b"download:00000003".to_vec(),
                b"abc".to_vec(),
                b"md5:900150983cd24fb0d6963f7d28e17f72".to_vec()
            ]
        );
    }

    #[test]
    fn list_partitions_merges_info_and_okay_fields_without_changing_names() {
        let mut client = FastbootClient::new(peer(&[
            b"INFOproduct:SemiDrive",
            b"INFOpartition-size:emmc1$$boot_a: 0x1000",
            b"INFOpartition-type:emmc1$$boot_a:raw",
            b"INFOis-logical:emmc1$$boot_a:no",
            b"INFOpartition-type:ospi1$$spl:raw",
            b"INFOpartition-size:bad-size:not-a-size",
            b"INFOpartition-size::0x10",
            b"OKAYpartition-size:ospi1$$spl:00000200",
        ]));
        let listing = client.list_partitions().unwrap();
        assert!(listing.warning.is_none());
        assert_eq!(
            listing.partitions,
            [
                PartitionInfo {
                    name: "bad-size".into(),
                    ..PartitionInfo::default()
                },
                PartitionInfo {
                    name: "emmc1$$boot_a".into(),
                    size: Some(4096),
                    kind: Some("raw".into()),
                    logical: Some(false)
                },
                PartitionInfo {
                    name: "ospi1$$spl".into(),
                    size: Some(512),
                    kind: Some("raw".into()),
                    logical: None
                },
            ]
        );
        assert_eq!(client.transport.writes, [b"getvar:all".to_vec()]);
    }

    #[test]
    fn list_partitions_keeps_partial_device_advertisements_after_fail() {
        let mut client = FastbootClient::new(peer(&[
            b"INFOpartition-size:boot:1000",
            b"FAILnot all variables available",
        ]));
        let listing = client.list_partitions().unwrap();
        assert_eq!(listing.partitions[0].name, "boot");
        assert_eq!(listing.partitions[0].size, Some(4096));
        assert!(listing.warning.unwrap().contains("not all variables"));
        assert_eq!(client.transport.writes, [b"getvar:all".to_vec()]);
    }

    #[test]
    fn list_partitions_reports_unsupported_or_empty_enumeration() {
        for responses in [
            vec![b"FAILunknown variable".as_slice()],
            vec![b"OKAY".as_slice()],
            vec![b"INFOproduct:SemiDrive".as_slice(), b"OKAY"],
        ] {
            let mut client = FastbootClient::new(peer(&responses));
            let error = client.list_partitions().unwrap_err();
            assert!(
                error.to_string().contains("may not support enumeration"),
                "{error:#}"
            );
            assert_eq!(client.transport.writes, [b"getvar:all".to_vec()]);
        }
    }

    #[test]
    fn list_partitions_does_not_mask_transport_errors_as_partial_success() {
        let mut client = FastbootClient::new(peer(&[b"INFOpartition-size:boot:0x1000"]));
        let error = client.list_partitions().unwrap_err();
        assert!(format!("{error:#}").contains("no queued response"));
    }

    #[test]
    fn forward_flash_reader_leaves_the_next_payload_unread() {
        let mut client =
            FastbootClient::new(peer(&[b"OKAY0x4", b"DATA00000003", b"OKAY", b"OKAY"]));
        let mut source = b"abcNEXT".as_slice();
        let mut progress = vec![];
        client
            .flash_reader_with_progress("kernel", &mut source, 3, false, |state| {
                progress.push(state);
            })
            .unwrap();
        assert_eq!(
            progress,
            [
                FlashProgress {
                    completed: 0,
                    total: 3,
                    transferred: 0
                },
                FlashProgress {
                    completed: 3,
                    total: 3,
                    transferred: 3
                },
            ]
        );
        assert_eq!(source, b"NEXT");
        assert!(client.transport.writes.contains(&b"flash:kernel".to_vec()));
    }

    #[test]
    fn command_limits_and_fail() {
        let mut client = FastbootClient::new(peer(&[b"FAILdenied"]));
        assert!(client.command("erase:x").is_err());
        assert!(client.send(&"x".repeat(65)).is_err());
    }

    #[test]
    fn segmented_only_waits_on_final_flash() {
        let mut client = FastbootClient::new(peer(&[
            b"OKAY0x2",
            b"DATA00000002",
            b"OKAY",
            b"DATA00000002",
            b"OKAY",
            b"OKAY",
        ]));
        let mut progress = vec![];
        client
            .flash_reader_with_progress("x", &mut &b"abcd"[..], 4, false, |state| {
                progress.push(state);
            })
            .unwrap();
        assert!(progress
            .iter()
            .any(|state| state.completed == 2 && state.transferred == 2));
        assert_eq!(
            progress.last(),
            Some(&FlashProgress {
                completed: 4,
                total: 4,
                transferred: 4
            })
        );
        assert!(client.transport.responses.is_empty());
        assert!(client
            .transport
            .writes
            .contains(&b"flash-seq:x:off0/4".to_vec()));
        assert!(client
            .transport
            .writes
            .contains(&b"flash-seq:x:off2/4".to_vec()));
    }

    #[test]
    fn readback_atomic_verification() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out");
        let mut client = FastbootClient::new(peer(&[
            b"DATA00000003",
            b"abc",
            b"MD5900150983cd24fb0d6963f7d28e17f72",
            b"OKAY",
        ]));
        client.readback("x", &output).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), b"abc");
    }

    #[test]
    fn readback_to_writer_preserves_binary_data_after_verification() {
        let bytes = b"\0\r\n\x1a\xff\x80";
        let md5 = format!("MD5{:x}", Md5::digest(bytes));
        let mut client =
            FastbootClient::new(peer(&[b"DATA00000006", bytes, md5.as_bytes(), b"OKAY"]));
        let mut output = vec![];
        assert_eq!(client.readback_to("boot", &mut output).unwrap(), 6);
        assert_eq!(output, bytes);
        assert_eq!(client.transport.writes, [b"read:boot:0:0".to_vec()]);
    }

    #[test]
    fn readback_to_writer_publishes_nothing_on_md5_or_final_response_failure() {
        for responses in [
            vec![
                b"DATA00000003".as_slice(),
                b"abc",
                b"MD500000000000000000000000000000000",
                b"OKAY",
            ],
            vec![
                b"DATA00000003".as_slice(),
                b"abc",
                b"MD5900150983cd24fb0d6963f7d28e17f72",
                b"FAILread failed",
            ],
            vec![b"DATA00000003".as_slice(), b"ab"],
        ] {
            let mut client = FastbootClient::new(peer(&responses));
            let mut output = b"unchanged".to_vec();
            assert!(client.readback_to("boot", &mut output).is_err());
            assert_eq!(output, b"unchanged");
        }
    }

    #[test]
    fn readback_to_writer_propagates_broken_pipe() {
        struct BrokenPipe;
        impl Write for BrokenPipe {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut client = FastbootClient::new(peer(&[
            b"DATA00000003",
            b"abc",
            b"MD5900150983cd24fb0d6963f7d28e17f72",
            b"OKAY",
        ]));
        let error = client.readback_to("boot", &mut BrokenPipe).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn partition_escaping() {
        assert_eq!(partition_name(r"emmc1\$\$foo-0x123"), "emmc1$$0x123");
    }

    #[test]
    fn sparse_offsets_and_trailing_zero_chunk() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xed26ff3au32.to_le_bytes());

        for value in [1u16, 0, 28, 12] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }

        for value in [4u32, 4, 5, 0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }

        let chunks: [(u16, u32, &[u8]); 5] = [
            (0xcac1, 1, b"abcd"),
            (0xcac3, 1, b""),
            (0xcac2, 1, b"1234"),
            (0xcac3, 1, b""),
            (0xcac1, 0, b""),
        ];

        for (kind, blocks, data) in chunks {
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&blocks.to_le_bytes());
            bytes.extend_from_slice(&(12 + data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(data);
        }

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        let mut client = FastbootClient::new(peer(&[
            b"OKAY0x4",
            b"DATA00000004",
            b"OKAY",
            b"OKAY",
            b"DATA00000004",
            b"OKAY",
            b"OKAY",
            b"DATA00000004",
            b"OKAY",
            b"OKAY",
            b"OKAY",
        ]));

        let mut progress = vec![];
        client
            .flash_reader_with_progress(
                "x",
                &mut std::fs::File::open(file.path()).unwrap(),
                file.as_file().metadata().unwrap().len(),
                true,
                |state| progress.push(state),
            )
            .unwrap();
        assert_eq!(
            progress.first(),
            Some(&FlashProgress {
                completed: 0,
                total: 16,
                transferred: 0
            })
        );
        assert!(progress
            .iter()
            .any(|state| state.completed == 8 && state.transferred == 4));
        assert_eq!(
            progress.last(),
            Some(&FlashProgress {
                completed: 16,
                total: 16,
                transferred: 12
            })
        );
        assert!(progress
            .windows(2)
            .all(|pair| pair[0].completed <= pair[1].completed));

        let writes = &client.transport.writes;
        assert!(writes.contains(&b"flash-seq:x:off0/10".to_vec()));
        assert!(writes.contains(&b"flash-seq:x:off8/10".to_vec()));
        assert!(writes.contains(&b"flash-seq:x:offc/10".to_vec()));
        assert!(writes.contains(&vec![0; 4]));
        assert!(writes.contains(&b"md5:f1d3ff8443297732862df21dc4e57262".to_vec()));
        assert!(client.transport.responses.is_empty());
    }

    #[test]
    fn extended_readback_lengths_are_64_bit() {
        let mut client = FastbootClient::new(peer(&[b"DATA100000000"]));
        assert_eq!(client.response().unwrap(), Response::Data(1u64 << 32));
    }

    #[test]
    fn flash_progress_does_not_hide_final_device_failure_or_truncated_input() {
        let mut client = FastbootClient::new(peer(&[
            b"OKAY0x4",
            b"DATA00000004",
            b"OKAY",
            b"FAILwrite failed",
        ]));
        let mut progress = vec![];
        let error = client
            .flash_reader_with_progress("x", &mut &b"abcd"[..], 4, false, |state| {
                progress.push(state)
            })
            .unwrap_err();
        assert!(error.to_string().contains("write failed"));
        assert_eq!(progress.last().unwrap().completed, 4);

        let mut client = FastbootClient::new(peer(&[b"OKAY0x5", b"DATA00000005"]));
        let mut progress = vec![];
        assert!(client
            .flash_reader_with_progress("x", &mut &b"abcd"[..], 5, false, |state| progress
                .push(state),)
            .is_err());
        assert_eq!(progress.last().unwrap().completed, 0);
        assert_eq!(progress.last().unwrap().transferred, 0);
    }

    #[test]
    fn flash_progress_counts_partial_usb_writes_before_a_transport_error() {
        struct PartialPeer(Peer);
        impl BulkTransport for PartialPeer {
            fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
                self.0.read(buffer)
            }
            fn write(&mut self, bytes: &[u8]) -> Result<usize> {
                match bytes {
                    b"abcd" => self.0.write(b"a"),
                    b"bcd" => bail!("USB disconnected"),
                    _ => self.0.write(bytes),
                }
            }
        }
        let mut client = FastbootClient::new(PartialPeer(peer(&[b"OKAY0x4", b"DATA00000004"])));
        let mut progress = vec![];
        let error = client
            .flash_reader_with_progress("x", &mut &b"abcd"[..], 4, false, |state| {
                progress.push(state)
            })
            .unwrap_err();
        assert!(error.to_string().contains("USB disconnected"));
        assert_eq!(
            progress.last(),
            Some(&FlashProgress {
                completed: 1,
                total: 4,
                transferred: 1,
            })
        );
        assert!(!client
            .transport
            .0
            .writes
            .iter()
            .any(|bytes| bytes.starts_with(b"flash:")));
    }

    #[test]
    fn malformed_limit_response_does_not_start_a_download() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"abcd").unwrap();
        let mut client = FastbootClient::new(peer(&[b"broken response"]));

        assert!(client.flash("x", file.path(), false).is_err());
        assert_eq!(
            client.transport.writes,
            [b"getvar:max-download-size".to_vec()]
        );
    }

    #[test]
    fn corrupt_readback_keeps_existing_output() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("readback.bin");
        std::fs::write(&output, b"existing").unwrap();
        let mut client = FastbootClient::new(peer(&[
            b"DATA00000003",
            b"abc",
            b"MD500000000000000000000000000000000",
            b"OKAY",
        ]));

        assert!(client.readback("x", &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"existing");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
