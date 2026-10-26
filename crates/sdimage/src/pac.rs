//! Bounds-checked, streaming PAC parsing. Offsets in nested PACs are relative
//! to their containing PAC, not to the outer file.
use crate::BZIP2;
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const MAGIC: u32 = 0x5041_434b;
pub const TABLE_END: u64 = 17_408;

/// PAC_FILE_TYPE from the SDK's src/unpac.py. Unknown IDs are retained so newer
/// resource types can still be inspected without losing their on-disk value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum PacFileType {
    Fda = 0,
    OspiDa = 1,
    Dloader = 2,
    Spl = 3,
    Sfs = 4,
    Gpt = 5,
    Img = 6,
    Pac = 7,
    PackFbl = 8,
    PackSbl = 9,
    PackTbl = 10,
    PackNfs = 11,
    PackRfd = 12,
    Unknown(u32),
}

impl From<u32> for PacFileType {
    fn from(value: u32) -> Self {
        match value {
            0 => Self::Fda,
            1 => Self::OspiDa,
            2 => Self::Dloader,
            3 => Self::Spl,
            4 => Self::Sfs,
            5 => Self::Gpt,
            6 => Self::Img,
            7 => Self::Pac,
            8 => Self::PackFbl,
            9 => Self::PackSbl,
            10 => Self::PackTbl,
            11 => Self::PackNfs,
            12 => Self::PackRfd,
            other => Self::Unknown(other),
        }
    }
}

impl PacFileType {
    pub const fn raw(self) -> u32 {
        match self {
            Self::Fda => 0,
            Self::OspiDa => 1,
            Self::Dloader => 2,
            Self::Spl => 3,
            Self::Sfs => 4,
            Self::Gpt => 5,
            Self::Img => 6,
            Self::Pac => 7,
            Self::PackFbl => 8,
            Self::PackSbl => 9,
            Self::PackTbl => 10,
            Self::PackNfs => 11,
            Self::PackRfd => 12,
            Self::Unknown(value) => value,
        }
    }

    pub const fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Fda => "FDA",
            Self::OspiDa => "OSPI_DA",
            Self::Dloader => "DLOADER",
            Self::Spl => "SPL",
            Self::Sfs => "SFS",
            Self::Gpt => "GPT",
            Self::Img => "IMG",
            Self::Pac => "PAC",
            Self::PackFbl => "PACK_FBL",
            Self::PackSbl => "PACK_SBL",
            Self::PackTbl => "PACK_TBL",
            Self::PackNfs => "PACK_NFS",
            Self::PackRfd => "PACK_RFD",
            Self::Unknown(_) => return None,
        })
    }
}

impl From<PacFileType> for u32 {
    fn from(value: PacFileType) -> Self {
        value.raw()
    }
}

impl std::fmt::Display for PacFileType {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            output,
            "{} ({})",
            self.name().unwrap_or("UNKNOWN"),
            self.raw()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub partition: String,
    pub kind: PacFileType,
    pub size: u64,
    pub offset: u64,
    pub slot_ab: bool,
    pub components: Vec<String>,
}

/// Compatibility helper for callers that have an undecoded raw type ID.
pub fn file_type_name(kind: u32) -> Option<&'static str> {
    PacFileType::from(kind).name()
}
#[derive(Debug)]
pub struct Pac {
    pub source: PathBuf,
    pub product: String,
    pub product_version: String,
    pub version: u32,
    pub entries: Vec<Entry>,
    pub empty: Vec<String>,
}

fn u32le(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n..n + 4].try_into().unwrap())
}

fn u64le(b: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(b[n..n + 8].try_into().unwrap())
}

fn text(bytes: &[u8]) -> Result<String> {
    let words: Vec<_> = bytes
        .chunks_exact(2)
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .take_while(|&v| v != 0)
        .collect();

    Ok(String::from_utf16(&words)
        .context("invalid UTF-16 in PAC")?
        .trim()
        .to_owned())
}

pub(crate) fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0']),
        "unsafe PAC name: {name:?}"
    );

    Ok(())
}

pub(crate) fn name_for(parents: &[String], leaf: &str, all: bool) -> String {
    if parents.is_empty() {
        leaf.into()
    } else if all && parents.len() == 1 {
        format!("{}$${leaf}", parents[0])
    } else {
        format!("{}${leaf}", parents.join("$"))
    }
}

impl Pac {
    pub fn open(path: impl AsRef<Path>, verify: bool) -> Result<Self> {
        let path = path.as_ref();
        let mut file = File::open(path).with_context(|| format!("open PAC {}", path.display()))?;
        let size = file.metadata()?.len();
        let mut header = [0; 1024];
        file.read_exact(&mut header)
            .context("PAC header is truncated")?;

        ensure!(u32le(&header, 0) == MAGIC, "invalid PAC magic");
        let version = u32le(&header, 4);

        ensure!(version <= 1, "unsupported PAC version {version}");
        let mut pac = Self {
            source: path.to_owned(),
            product: text(&header[8..264])?,
            product_version: text(&header[264..520])?,
            version,
            entries: vec![],
            empty: vec![],
        };
        pac.walk(&mut file, 0, size, &[], verify)?;

        Ok(pac)
    }

    fn walk(
        &mut self,
        file: &mut File,
        base: u64,
        len: u64,
        parents: &[String],
        verify: bool,
    ) -> Result<()> {
        let depth = parents.len();
        let all = self.version == 1;

        ensure!(
            depth < 32 && len >= TABLE_END,
            "invalid PAC nesting or container size"
        );
        let end = base.checked_add(len).context("PAC offset overflow")?;

        ensure!(end <= file.metadata()?.len(), "PAC exceeds its source file");
        file.seek(SeekFrom::Start(base))?;
        let mut header = [0; 1024];
        file.read_exact(&mut header)?;

        ensure!(u32le(&header, 0) == MAGIC, "invalid nested PAC magic");
        let version = u32le(&header, 4);

        ensure!(
            version == 0 || (depth == 0 && version == 1),
            "invalid nested PAC version"
        );
        let count = u32le(&header, 528) as usize;

        ensure!(count <= 128, "PAC table exceeds 128 entries");

        if verify {
            let expected = u32le(&header, 1020);

            if expected != 0 && expected != u32::MAX {
                let mut digest = BZIP2.digest();
                digest.update(&header[..1020]);
                let mut remaining = len - 1024;
                let mut buffer = vec![0; 1024 * 1024];

                while remaining > 0 {
                    let n = remaining.min(buffer.len() as u64) as usize;
                    file.read_exact(&mut buffer[..n])?;
                    digest.update(&buffer[..n]);
                    remaining -= n as u64;
                }

                ensure!(
                    digest.finalize() == expected,
                    "PAC CRC mismatch at offset {base:#x}"
                );
            }
        }
        file.seek(SeekFrom::Start(base + 1024))?;
        let mut table = vec![0; count * 128];
        file.read_exact(&mut table)?;

        for raw in table.chunks_exact(128) {
            let mut name = text(&raw[..72])?;
            valid_name(&name)?;
            let size = u64le(raw, 72);
            let relative = u64le(raw, 80);
            let kind = PacFileType::from(u32le(raw, 88));
            let slot_ab = name.ends_with("_a");

            if slot_ab {
                name.truncate(name.len() - 2);
                valid_name(&name)?;
            }

            if size == 0 {
                self.empty.push(name_for(parents, &name, all));
                continue;
            }

            ensure!(
                relative >= TABLE_END && relative.checked_add(size).is_some_and(|n| n <= len),
                "partition {name} exceeds its PAC container"
            );
            let offset = base.checked_add(relative).context("PAC offset overflow")?;

            if kind == PacFileType::Pac || (depth == 0 && version == 1) {
                // Original version-1 containers may contain empty placeholders.
                if size < TABLE_END {
                    continue;
                }

                let mut children = parents.to_vec();
                children.push(name);
                self.walk(file, offset, size, &children, verify)?;
            } else {
                let mut components = parents.to_vec();
                components.push(format!("{name}.bin"));
                self.entries.push(Entry {
                    partition: name_for(parents, &name, all),
                    kind,
                    size,
                    offset,
                    slot_ab,
                    components,
                });
            }
        }

        Ok(())
    }

    /// Atomically extract without traversing symlinks or overwriting unrelated
    /// files. Identical existing files can be reused across multiple PACs.
    pub fn extract(&self, output: impl AsRef<Path>) -> Result<Vec<(Entry, PathBuf)>> {
        self.extract_with_progress(output, |_, _| {})
    }

    /// Extract files, reporting each entry and destination before copying it.
    /// Identical existing files are also reported before checking their contents.
    pub fn extract_with_progress(
        &self,
        output: impl AsRef<Path>,
        mut progress: impl FnMut(&Entry, &Path),
    ) -> Result<Vec<(Entry, PathBuf)>> {
        let output = output.as_ref();
        fs::create_dir_all(output)?;
        let root = output.canonicalize()?;
        let mut source = File::open(&self.source)?;
        let mut result = vec![];

        for entry in &self.entries {
            let mut dir = root.clone();

            for component in &entry.components[..entry.components.len() - 1] {
                valid_name(component)?;
                dir.push(component);

                if dir.symlink_metadata().is_ok() {
                    ensure!(
                        !dir.symlink_metadata()?.file_type().is_symlink(),
                        "symlink in extraction path"
                    );
                }
                fs::create_dir_all(&dir)?;

                ensure!(
                    dir.canonicalize()?.starts_with(&root),
                    "PAC path escapes output"
                );
            }

            let name = entry.components.last().unwrap();
            valid_name(name)?;
            let path = dir.join(name);
            progress(entry, &path);
            source.seek(SeekFrom::Start(entry.offset))?;
            let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
            let copied = std::io::copy(&mut (&mut source).take(entry.size), &mut temporary)?;

            ensure!(
                copied == entry.size,
                "truncated partition {}",
                entry.partition
            );
            temporary.flush()?;

            if path.symlink_metadata().is_ok() {
                ensure!(
                    path.symlink_metadata()?.is_file(),
                    "extraction destination is not a regular file"
                );

                ensure!(
                    path.metadata()?.len() == entry.size
                        && crate::md5_reader(File::open(&path)?)?
                            == crate::md5_reader(temporary.reopen()?)?,
                    "output already exists with different data: {}",
                    path.display()
                );
            } else if let Err(error) = temporary.persist_noclobber(&path) {
                bail!("publish {}: {}", path.display(), error.error);
            }
            result.push((entry.clone(), path));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_enum_round_trips_sdk_values_and_unknown_ids() {
        let known = [
            PacFileType::Fda,
            PacFileType::OspiDa,
            PacFileType::Dloader,
            PacFileType::Spl,
            PacFileType::Sfs,
            PacFileType::Gpt,
            PacFileType::Img,
            PacFileType::Pac,
            PacFileType::PackFbl,
            PacFileType::PackSbl,
            PacFileType::PackTbl,
            PacFileType::PackNfs,
            PacFileType::PackRfd,
        ];
        for (raw, kind) in known.into_iter().enumerate() {
            assert_eq!(PacFileType::from(raw as u32), kind);
            assert_eq!(kind.raw(), raw as u32);
            assert_eq!(u32::from(kind), raw as u32);
            assert!(kind.name().is_some());
        }
        for raw in [13, 99, u32::MAX] {
            let kind = PacFileType::from(raw);
            assert_eq!(kind, PacFileType::Unknown(raw));
            assert_eq!(kind.raw(), raw);
            assert_eq!(kind.name(), None);
            assert_eq!(kind.to_string(), format!("UNKNOWN ({raw})"));
        }
    }

    #[test]
    fn both_pac_readers_preserve_unknown_resource_types() {
        let bytes = fixture("future", b"payload", 99);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("future.pac");
        fs::write(&path, &bytes).unwrap();
        let pac = Pac::open(path, true).unwrap();
        assert_eq!(pac.entries[0].kind, PacFileType::Unknown(99));
        let mut stream =
            crate::stream::PacStream::with_resource_metadata(bytes.as_slice(), true).unwrap();
        assert_eq!(
            stream.next_entry().unwrap().unwrap().kind,
            PacFileType::Unknown(99)
        );
        stream.finish().unwrap();
        assert_eq!(stream.resources[0].kind, PacFileType::Unknown(99));
    }

    fn fixture(name: &str, data: &[u8], kind: u32) -> Vec<u8> {
        let mut bytes = vec![0; TABLE_END as usize];
        bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[528..532].copy_from_slice(&1u32.to_le_bytes());

        for (i, word) in name.encode_utf16().enumerate() {
            bytes[1024 + i * 2..1026 + i * 2].copy_from_slice(&word.to_le_bytes());
        }
        bytes[1096..1104].copy_from_slice(&(data.len() as u64).to_le_bytes());
        bytes[1104..1112].copy_from_slice(&TABLE_END.to_le_bytes());
        bytes[1112..1116].copy_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn extract_nested_and_ab() {
        let inner = fixture("kernel_a", b"payload", 6);
        let outer = fixture("ap1", &inner, 7);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("fw.pac");
        fs::write(&source, outer).unwrap();
        let pac = Pac::open(&source, true).unwrap();
        assert_eq!(pac.entries[0].partition, "ap1$kernel");
        assert!(pac.entries[0].slot_ab);
        let mut reported = vec![];
        let result = pac
            .extract_with_progress(dir.path().join("out"), |entry, path| {
                assert!(!path.exists());
                reported.push((entry.clone(), path.to_owned()));
            })
            .unwrap();

        assert_eq!(reported, result);
        assert_eq!(fs::read(&result[0].1).unwrap(), b"payload");
    }

    #[test]
    fn crc_and_bounds() {
        assert_eq!(BZIP2.checksum(b"123456789"), 0xfc891918);
        let mut bytes = fixture("image", b"data", 6);
        let mut digest = BZIP2.digest();
        digest.update(&bytes[..1020]);
        digest.update(&bytes[1024..]);
        bytes[1020..1024].copy_from_slice(&digest.finalize().to_le_bytes());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        fs::write(&path, &bytes).unwrap();
        assert!(Pac::open(&path, true).is_ok());
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(Pac::open(&path, true).is_err());
    }

    #[test]
    fn reject_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        fs::write(&path, fixture("../bad", b"x", 6)).unwrap();
        assert!(Pac::open(path, false).is_err());
    }
}
