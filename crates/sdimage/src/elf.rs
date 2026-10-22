//! Flashloader ELF parsing through the `object` crate (ELF32 and ELF64).
use anyhow::{ensure, Context, Result};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SectionFlags};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Debug, Clone, Default)]
pub struct FlashDevice {
    pub base: u64,
    pub size: u64,
    pub page_size: u32,
    pub erased: u8,
    pub program_timeout_ms: u32,
    pub erase_timeout_ms: u32,
    pub sectors: Vec<(u32, u32)>,
}
#[derive(Debug)]
pub struct Loader {
    pub base: u64,
    pub data: Vec<u8>,
    pub symbols: BTreeMap<String, u64>,
    pub flash: FlashDevice,
    pub version: String,
    pub is_64: bool,
}

fn word(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .context("truncated flash device descriptor")?
            .try_into()?,
    ))
}

impl FlashDevice {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= 157, "truncated DevDscr section");
        let natural = bytes.get(149..152).is_some_and(|b| b == [0, 0, 0])
            && word(bytes, 152).is_ok_and(|v| (1..=60000).contains(&v));
        let packed = word(bytes, 149).is_ok_and(|v| (1..=60000).contains(&v));
        let at = if natural || !packed { 152 } else { 149 };
        let mut sectors = vec![];

        for i in 0..4 {
            let offset = at + 8 + i * 8;

            if offset + 8 <= bytes.len() {
                sectors.push((word(bytes, offset)?, word(bytes, offset + 4)?));
            }
        }

        Ok(Self {
            base: word(bytes, 132)? as u64,
            size: word(bytes, 136)? as u64,
            page_size: word(bytes, 140)?,
            erased: bytes[148],
            program_timeout_ms: word(bytes, at)?,
            erase_timeout_ms: word(bytes, at + 4)?,
            sectors,
        })
    }

    pub fn sector_size(&self) -> Result<u64> {
        let size = self.sectors.first().map(|s| s.0).unwrap_or(0);

        ensure!(
            size > 0 && size != u32::MAX,
            "invalid flash sector geometry"
        );

        Ok(size as u64)
    }
}

impl Loader {
    pub fn is_elf(path: impl AsRef<Path>) -> Result<bool> {
        use std::io::Read;
        let mut head = [0; 4];
        let mut file = fs::File::open(path)?;

        Ok(file.read(&mut head)? == 4 && head == *b"\x7fELF")
    }

    pub fn parse(
        path: impl AsRef<Path>,
        size_override: Option<usize>,
        arm_instructions: bool,
    ) -> Result<Self> {
        let bytes = fs::read(path)?;
        let file = object::File::parse(&*bytes).context("invalid loader ELF")?;

        ensure!(
            file.is_little_endian(),
            "only little-endian SemiDrive loaders are supported"
        );
        let base = file
            .segments()
            .map(|s| s.address())
            .min()
            .context("ELF has no loadable segments")?;
        let mut end = base;

        for segment in file.segments() {
            end = end.max(
                segment
                    .address()
                    .checked_add(segment.size())
                    .context("ELF segment overflow")?,
            )
        }

        for section in file.sections() {
            if matches!(section.flags(),SectionFlags::Elf{sh_flags} if sh_flags&2!=0) {
                end = end.max(
                    section
                        .address()
                        .checked_add(section.size())
                        .context("ELF section overflow")?,
                )
            }
        }

        let size = size_override.unwrap_or(usize::try_from(
            (end - base).checked_add(3).context("ELF size overflow")? & !3,
        )?);

        ensure!(
            size > 0 && size <= 512 * 1024 * 1024,
            "invalid/excessive flashloader size"
        );
        let mut data = vec![0; size];

        for segment in file.segments() {
            let offset = usize::try_from(
                segment
                    .address()
                    .checked_sub(base)
                    .context("ELF segment below image base")?,
            )?;
            let source = segment.data()?;

            if offset < data.len() {
                let n = source.len().min(data.len() - offset);
                data[offset..offset + n].copy_from_slice(&source[..n]);
            }
        }

        let mut symbols = BTreeMap::new();

        for symbol in file.symbols() {
            if !symbol.is_undefined() {
                if let Ok(name) = symbol.name() {
                    if !name.is_empty() {
                        let value = if arm_instructions {
                            symbol.address() & !1
                        } else {
                            symbol.address()
                        };
                        symbols.insert(name.to_owned(), value);
                    }
                }
            }
        }

        let flash = match file.section_by_name("DevDscr") {
            Some(s) => FlashDevice::parse(s.data()?)?,
            None => FlashDevice::default(),
        };
        let version = file
            .section_by_name("version")
            .map(|s| {
                s.data()
                    .map(|d| String::from_utf8_lossy(d).trim_end_matches('\0').to_owned())
            })
            .transpose()?
            .unwrap_or_default();

        Ok(Self {
            base,
            data,
            symbols,
            flash,
            version,
            is_64: file.is_64(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put16(bytes: &mut [u8], at: usize, value: u16) {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put64(bytes: &mut [u8], at: usize, value: u64) {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn executable(wide: bool) -> Vec<u8> {
        let mut bytes = vec![0; 768];
        let base = 0x400000;

        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = if wide { 2 } else { 1 };
        bytes[5] = 1;
        bytes[6] = 1;
        put16(&mut bytes, 16, 2);
        put16(&mut bytes, 18, if wide { 183 } else { 40 });
        put32(&mut bytes, 20, 1);

        if wide {
            put64(&mut bytes, 24, base);
            put64(&mut bytes, 32, 64);
            put64(&mut bytes, 40, 512);
            put16(&mut bytes, 52, 64);
            put16(&mut bytes, 54, 56);
            put16(&mut bytes, 56, 1);
            put16(&mut bytes, 58, 64);
            put16(&mut bytes, 60, 4);
            put16(&mut bytes, 62, 3);

            put32(&mut bytes, 64, 1);
            put32(&mut bytes, 68, 5);

            for (offset, value) in [
                (72, 256),
                (80, base),
                (88, base),
                (96, 4),
                (104, 8),
                (112, 4),
            ] {
                put64(&mut bytes, offset, value);
            }
        } else {
            put32(&mut bytes, 24, base as u32);
            put32(&mut bytes, 28, 52);
            put32(&mut bytes, 32, 512);
            put16(&mut bytes, 40, 52);
            put16(&mut bytes, 42, 32);
            put16(&mut bytes, 44, 1);
            put16(&mut bytes, 46, 40);
            put16(&mut bytes, 48, 4);
            put16(&mut bytes, 50, 3);

            for (offset, value) in [
                (52, 1),
                (56, 256),
                (60, base as u32),
                (64, base as u32),
                (68, 4),
                (72, 8),
                (76, 5),
                (80, 4),
            ] {
                put32(&mut bytes, offset, value);
            }
        }

        bytes[256..260].copy_from_slice(&[1, 2, 3, 4]);
        let strings = b"\0.text\0.bss\0.shstrtab\0";
        bytes[300..300 + strings.len()].copy_from_slice(strings);

        for (index, name, kind, flags, address, offset, size) in [
            (1, 1, 1, 6, base, 256, 4),
            (2, 7, 8, 3, base + 4, 260, 4),
            (3, 12, 3, 0, 0, 300, strings.len() as u64),
        ] {
            let at = 512 + index * if wide { 64 } else { 40 };
            put32(&mut bytes, at, name);
            put32(&mut bytes, at + 4, kind);

            if wide {
                for (field, value) in [(8, flags), (16, address), (24, offset), (32, size), (48, 1)]
                {
                    put64(&mut bytes, at + field, value);
                }
            } else {
                for (field, value) in [(8, flags), (12, address), (16, offset), (20, size), (32, 1)]
                {
                    put32(&mut bytes, at + field, value as u32);
                }
            }
        }

        bytes
    }

    #[test]
    fn elf32_and_elf64_expand_bss_without_external_tools() {
        for wide in [false, true] {
            let file = tempfile::NamedTempFile::new().unwrap();
            fs::write(file.path(), executable(wide)).unwrap();

            let image = Loader::parse(file.path(), None, true).unwrap();

            assert_eq!(image.base, 0x400000);
            assert_eq!(image.is_64, wide);
            assert_eq!(image.data, [1, 2, 3, 4, 0, 0, 0, 0]);
        }
    }

    #[test]
    fn rejects_truncated_or_unbounded_elf_images() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"\x7fELF").unwrap();
        assert!(Loader::parse(file.path(), None, true).is_err());

        fs::write(file.path(), executable(false)).unwrap();
        assert!(Loader::parse(file.path(), Some(usize::MAX), true).is_err());
    }

    #[test]
    fn packed_and_aligned_flash_descriptors() {
        for aligned in [false, true] {
            let mut bytes = vec![0; 192];
            put32(&mut bytes, 136, 1024 * 1024);
            put32(&mut bytes, 140, 256);
            bytes[148] = 0xff;

            let start = if aligned { 152 } else { 149 };
            put32(&mut bytes, start, 100);
            put32(&mut bytes, start + 4, 1000);
            put32(&mut bytes, start + 8, 4096);

            let descriptor = FlashDevice::parse(&bytes).unwrap();

            assert_eq!(descriptor.page_size, 256);
            assert_eq!(descriptor.sector_size().unwrap(), 4096);
            assert_eq!(descriptor.erased, 0xff);
        }
    }
}
