//! Forward-only PAC traversal. Payloads are read once; checksums are checked
//! when their containing PAC ends, including bytes skipped between entries.
use crate::{pac::Entry, BZIP2};
use anyhow::{ensure, Context, Result};
use std::{
    collections::VecDeque,
    io::{self, Read},
};

struct Item {
    entry: Entry,
    container: bool,
}

struct Frame {
    base: u64,
    end: Option<u64>,
    pending: VecDeque<Item>,
    expected: u32,
    digest: crc::Digest<'static, u32>,
}

pub struct PacStream<R> {
    reader: R,
    position: u64,
    remaining: u64,
    frames: Vec<Frame>,
    verify: bool,
    pub version: u32,
    pub product: String,
    pub empty: Vec<String>,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn long(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn text(bytes: &[u8]) -> Result<String> {
    let words: Vec<_> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|&b| b != 0)
        .collect();
    Ok(String::from_utf16(&words)
        .context("invalid PAC UTF-16")?
        .trim()
        .to_owned())
}

impl<R: Read> PacStream<R> {
    pub fn new(reader: R, verify: bool) -> Result<Self> {
        let mut stream = Self {
            reader,
            position: 0,
            remaining: 0,
            frames: vec![],
            verify,
            version: 0,
            product: String::new(),
            empty: vec![],
        };
        stream.container(None, vec![])?;
        Ok(stream)
    }

    fn raw(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = loop {
            match self.reader.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        self.position = self
            .position
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("PAC position overflow"))?;
        if self.verify {
            for frame in &mut self.frames {
                frame.digest.update(&buffer[..count]);
            }
        }
        Ok(count)
    }

    fn exact(&mut self, mut buffer: &mut [u8]) -> Result<()> {
        while !buffer.is_empty() {
            let count = self.raw(buffer)?;
            ensure!(count > 0, "truncated PAC at offset {:#x}", self.position);
            buffer = &mut buffer[count..];
        }
        Ok(())
    }

    fn advance(&mut self, end: u64) -> Result<()> {
        ensure!(
            end >= self.position,
            "PAC requires a backward read or contains overlapping entries"
        );
        let mut buffer = [0; 65536];
        while self.position < end {
            let count = (end - self.position).min(buffer.len() as u64) as usize;
            self.exact(&mut buffer[..count])?;
        }
        Ok(())
    }

    fn container(&mut self, end: Option<u64>, parents: Vec<String>) -> Result<()> {
        use crate::pac::{MAGIC, TABLE_END};
        ensure!(parents.len() < 32, "PAC nesting exceeds limit");
        let base = self.position;
        if let Some(end) = end {
            ensure!(
                end.checked_sub(base).is_some_and(|n| n >= TABLE_END),
                "invalid PAC container size"
            );
        }
        let mut header = [0; 1024];
        self.exact(&mut header)?;
        ensure!(word(&header, 0) == MAGIC, "invalid PAC magic");
        let version = word(&header, 4);
        ensure!(
            version == 0 || (parents.is_empty() && version == 1),
            "invalid PAC version"
        );
        if parents.is_empty() {
            self.version = version;
            self.product = text(&header[8..264])?;
        }
        let count = word(&header, 528) as usize;
        ensure!(count <= 128, "PAC table exceeds 128 entries");
        let mut digest = BZIP2.digest();
        digest.update(&header[..1020]);
        self.frames.push(Frame {
            base,
            end,
            pending: VecDeque::new(),
            expected: word(&header, 1020),
            digest,
        });
        let mut table = vec![0; count * 128];
        self.exact(&mut table)?;
        let mut items = vec![];
        for row in table.chunks_exact(128) {
            let mut name = text(&row[..72])?;
            super::pac::valid_name(&name)?;
            let slot_ab = name.ends_with("_a");
            if slot_ab {
                name.truncate(name.len() - 2);
                super::pac::valid_name(&name)?;
            }
            let partition = super::pac::name_for(&parents, &name, self.version == 1);
            let size = long(row, 72);
            if size == 0 {
                self.empty.push(partition);
                continue;
            }
            let relative = long(row, 80);
            let offset = base.checked_add(relative).context("PAC offset overflow")?;
            let entry_end = offset.checked_add(size).context("PAC size overflow")?;
            ensure!(
                relative >= TABLE_END && end.is_none_or(|end| entry_end <= end),
                "partition {partition} exceeds PAC bounds"
            );
            let kind = word(row, 88);
            let container = kind == 7 || (parents.is_empty() && version == 1);
            if container && size < TABLE_END && self.version == 1 {
                continue;
            }
            let mut components = parents.clone();
            components.push(if container {
                name
            } else {
                format!("{name}.bin")
            });
            items.push(Item {
                entry: Entry {
                    partition,
                    kind,
                    size,
                    offset,
                    slot_ab,
                    components,
                },
                container,
            });
        }
        items.sort_by_key(|item| item.entry.offset);
        let mut previous_end = base + TABLE_END;
        for item in &items {
            ensure!(item.entry.offset >= previous_end, "overlapping PAC entries");
            previous_end = item.entry.offset + item.entry.size;
        }
        self.frames.last_mut().unwrap().pending = items.into();
        self.advance(base + TABLE_END)?;
        Ok(())
    }

    /// Metadata for leaf entries already discovered in enclosing tables.
    pub fn pending_entries(&self) -> impl Iterator<Item = &Entry> {
        self.frames
            .iter()
            .flat_map(|frame| frame.pending.iter())
            .filter(|item| !item.container)
            .map(|item| &item.entry)
    }

    /// Nested headers still ahead of the current payload may reveal loaders.
    pub fn has_pending_containers(&self) -> bool {
        self.frames
            .iter()
            .any(|frame| frame.pending.iter().any(|item| item.container))
    }

    pub fn next_entry(&mut self) -> Result<Option<Entry>> {
        io::copy(self, &mut io::sink())?;
        loop {
            let Some(frame) = self.frames.last_mut() else {
                return Ok(None);
            };
            if let Some(item) = frame.pending.pop_front() {
                self.advance(item.entry.offset)?;
                if item.container {
                    self.container(
                        Some(item.entry.offset + item.entry.size),
                        item.entry.components,
                    )?;
                    continue;
                }
                self.remaining = item.entry.size;
                return Ok(Some(item.entry));
            }
            if let Some(end) = frame.end {
                self.advance(end)?;
            } else {
                let mut buffer = [0; 65536];
                while self.raw(&mut buffer)? != 0 {}
            }
            let frame = self.frames.pop().unwrap();
            if self.verify && frame.expected != 0 && frame.expected != u32::MAX {
                ensure!(
                    frame.digest.finalize() == frame.expected,
                    "PAC CRC mismatch at offset {:#x}",
                    frame.base
                );
            }
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        while self.next_entry()?.is_some() {}
        Ok(())
    }
}

impl<R: Read> Read for PacStream<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.remaining.min(buffer.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let count = self.raw(&mut buffer[..count])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated PAC payload",
            ));
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pac::{MAGIC, TABLE_END};

    pub(crate) fn packet(version: u32, entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut bytes = vec![0; TABLE_END as usize];
        bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
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
        checksum(&mut bytes);
        bytes
    }

    fn checksum(bytes: &mut [u8]) {
        let mut digest = BZIP2.digest();
        digest.update(&bytes[..1020]);
        digest.update(&bytes[1024..]);
        bytes[1020..1024].copy_from_slice(&digest.finalize().to_le_bytes());
    }

    struct Short<'a>(&'a [u8]);
    impl Read for Short<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = buffer.len().min(3);
            self.0.read(&mut buffer[..count])
        }
    }

    #[test]
    fn interrupted_reads_do_not_change_payload_boundaries_or_crc() {
        struct Interrupted<'a> {
            bytes: &'a [u8],
            interrupt: bool,
        }
        impl Read for Interrupted<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.interrupt = !self.interrupt;
                if self.interrupt {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = buffer.len().min(3);
                self.bytes.read(&mut buffer[..count])
            }
        }
        let bytes = packet(0, &[("kernel", 6, b"payload"), ("rootfs", 6, b"data")]);
        let mut stream = PacStream::new(
            Interrupted {
                bytes: &bytes,
                interrupt: false,
            },
            true,
        )
        .unwrap();
        assert_eq!(stream.next_entry().unwrap().unwrap().partition, "kernel");
        let mut payload = vec![];
        stream.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, b"payload");
        assert_eq!(stream.next_entry().unwrap().unwrap().partition, "rootfs");
        stream.finish().unwrap();
    }

    #[test]
    fn nested_forward_reads_preserve_boundaries_and_crc() {
        let child = packet(0, &[("kernel_a", 6, b"payload"), ("misc", 6, b"")]);
        let bytes = packet(1, &[("emmc1", 7, &child)]);
        let mut pac = PacStream::new(Short(&bytes), true).unwrap();
        let entry = pac.next_entry().unwrap().unwrap();
        assert_eq!(entry.partition, "emmc1$$kernel");
        assert!(entry.slot_ab);
        let mut payload = vec![];
        pac.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, b"payload");
        assert!(pac.next_entry().unwrap().is_none());
        assert_eq!(pac.empty, ["emmc1$$misc"]);
    }

    #[test]
    fn crc_is_deferred_but_nested_corruption_is_not_hidden() {
        let mut child = packet(0, &[("kernel", 6, b"payload")]);
        *child.last_mut().unwrap() ^= 1;
        let bytes = packet(0, &[("ap1", 7, &child)]);
        let mut pac = PacStream::new(bytes.as_slice(), true).unwrap();
        assert!(pac.next_entry().unwrap().is_some());
        let mut payload = vec![];
        pac.read_to_end(&mut payload).unwrap();
        assert_eq!(payload.len(), 7);
        let error = pac.next_entry().unwrap_err().to_string();
        assert!(error.contains("CRC mismatch"));
        let mut unchecked = PacStream::new(bytes.as_slice(), false).unwrap();
        unchecked.finish().unwrap();
    }

    #[test]
    fn table_order_can_differ_from_physical_order() {
        let mut bytes = packet(0, &[("first", 6, b"abc"), ("second", 6, b"def")]);
        for index in 0..128 {
            bytes.swap(1024 + index, 1152 + index);
        }
        checksum(&mut bytes);
        let mut pac = PacStream::new(bytes.as_slice(), true).unwrap();
        assert_eq!(pac.next_entry().unwrap().unwrap().partition, "first");
        // Advancing drains unread payloads without seeking or crossing entries.
        assert_eq!(pac.next_entry().unwrap().unwrap().partition, "second");
        pac.finish().unwrap();
    }

    #[test]
    fn truncated_and_overlapping_payloads_are_rejected() {
        let mut bytes = packet(0, &[("first", 6, b"abc"), ("second", 6, b"def")]);
        bytes.pop();
        let mut pac = PacStream::new(bytes.as_slice(), false).unwrap();
        assert!(pac.finish().is_err());
        let offset = bytes[1104..1112].to_vec();
        bytes[1232..1240].copy_from_slice(&offset);
        assert!(PacStream::new(bytes.as_slice(), false).is_err());
        assert!(PacStream::new(packet(0, &[("../bad", 6, b"x")]).as_slice(), false).is_err());
    }
}
