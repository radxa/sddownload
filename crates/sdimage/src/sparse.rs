//! A seekable expanded view of Android sparse images. Data is expanded into
//! caller buffers, never into an allocation proportional to the image size.
use anyhow::{ensure, Context, Result};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Debug, Clone)]
pub enum Contents {
    Raw(u64),
    Fill([u8; 4]),
    Zero,
}
#[derive(Debug, Clone)]
pub struct Span {
    pub offset: u64,
    pub size: u64,
    pub contents: Contents,
}

pub struct Image {
    file: File,
    pub size: u64,
    pub sparse: bool,
    pub spans: Vec<Span>,
    position: u64,
}

fn u16le(b: &[u8], n: usize) -> u16 {
    u16::from_le_bytes(b[n..n + 2].try_into().unwrap())
}

fn u32le(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n..n + 4].try_into().unwrap())
}

impl Image {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let mut magic = [0; 4];
        let sparse = length >= 4 && {
            file.read_exact(&mut magic)?;
            u32::from_le_bytes(magic) == 0xed26ff3a
        };

        if !sparse {
            return Ok(Self {
                file,
                size: length,
                sparse: false,
                spans: vec![Span {
                    offset: 0,
                    size: length,
                    contents: Contents::Raw(0),
                }],
                position: 0,
            });
        }
        file.seek(SeekFrom::Start(0))?;
        let mut header = [0; 28];
        file.read_exact(&mut header)?;

        ensure!(u16le(&header, 4) == 1, "unsupported sparse version");
        let fh = u16le(&header, 8) as u64;
        let ch = u16le(&header, 10) as u64;
        let block = u32le(&header, 12) as u64;

        ensure!(
            fh >= 28 && ch >= 12 && block > 0 && block.is_multiple_of(4) && fh <= length,
            "invalid sparse geometry"
        );
        let size = block
            .checked_mul(u32le(&header, 16) as u64)
            .context("sparse image overflow")?;
        let chunks = u32le(&header, 20);

        ensure!(
            u64::from(chunks) <= length / ch,
            "sparse chunk count exceeds file size"
        );
        file.seek(SeekFrom::Start(fh))?;
        let mut spans = vec![];
        let mut logical = 0;

        for _ in 0..chunks {
            let start = file.stream_position()?;
            let mut h = [0; 12];
            file.read_exact(&mut h)?;
            let kind = u16le(&h, 0);
            let expanded = block
                .checked_mul(u32le(&h, 4) as u64)
                .context("chunk overflow")?;
            let total = u32le(&h, 8) as u64;

            ensure!(
                total >= ch && start.checked_add(total).is_some_and(|v| v <= length),
                "sparse chunk exceeds file"
            );
            file.seek(SeekFrom::Start(start + ch))?;
            let contents = match kind {
                0xcac1 => {
                    ensure!(total - ch == expanded, "RAW chunk length mismatch");
                    Some(Contents::Raw(start + ch))
                }
                0xcac2 => {
                    ensure!(total - ch == 4, "FILL chunk length mismatch");
                    let mut fill = [0; 4];
                    file.read_exact(&mut fill)?;
                    Some(Contents::Fill(fill))
                }
                0xcac3 => {
                    ensure!(total == ch, "DONT_CARE payload is not empty");
                    Some(Contents::Zero)
                }
                0xcac4 => {
                    ensure!(expanded == 0 && total - ch == 4, "invalid CRC chunk");
                    None
                }
                _ => anyhow::bail!("unknown sparse chunk {kind:#x}"),
            };

            if let Some(contents) = contents {
                if expanded != 0 {
                    spans.push(Span {
                        offset: logical,
                        size: expanded,
                        contents,
                    });
                }
                logical = logical
                    .checked_add(expanded)
                    .context("expanded image overflow")?;
            }
            file.seek(SeekFrom::Start(start + total))?;
        }

        ensure!(logical == size, "sparse expanded size mismatch");

        Ok(Self {
            file,
            size,
            sparse: true,
            spans,
            position: 0,
        })
    }

    /// Interior holes aren't sent to flash-seq; a trailing hole is sent as zeros.
    pub fn transfer_ranges(&self) -> Vec<(u64, u64)> {
        self.spans
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                s.size > 0 && (!matches!(s.contents, Contents::Zero) || *i + 1 == self.spans.len())
            })
            .map(|(_, s)| (s.offset, s.size))
            .collect()
    }
}

impl Read for Image {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.size || buffer.is_empty() {
            return Ok(0);
        }

        let index = self
            .spans
            .partition_point(|s| s.offset + s.size <= self.position);
        let span = self
            .spans
            .get(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unmapped sparse data"))?;
        let within = self.position - span.offset;
        let n = (span.size - within).min(buffer.len() as u64) as usize;

        match span.contents {
            Contents::Raw(offset) => {
                self.file.seek(SeekFrom::Start(offset + within))?;
                self.file.read_exact(&mut buffer[..n])?;
            }
            Contents::Fill(fill) => {
                for (i, b) in buffer[..n].iter_mut().enumerate() {
                    *b = fill[((within + i as u64) % 4) as usize]
                }
            }
            Contents::Zero => buffer[..n].fill(0),
        }
        self.position += n as u64;

        Ok(n)
    }
}

impl Seek for Image {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(v) => i128::from(v),
            SeekFrom::Current(v) => i128::from(self.position) + i128::from(v),
            SeekFrom::End(v) => i128::from(self.size) + i128::from(v),
        };

        if next < 0 || next > i128::from(u64::MAX) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"));
        }
        self.position = next as u64;

        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fill_holes_and_seeking() {
        let mut bytes = vec![];
        bytes.extend(0xed26ff3au32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(0u16.to_le_bytes());
        bytes.extend(28u16.to_le_bytes());
        bytes.extend(12u16.to_le_bytes());

        for v in [4u32, 3, 3, 0] {
            bytes.extend(v.to_le_bytes())
        }

        for (kind, payload) in [
            (0xcac1u16, b"abcd".as_slice()),
            (0xcac3, &[]),
            (0xcac2, b"1234".as_slice()),
        ] {
            bytes.extend(kind.to_le_bytes());
            bytes.extend(0u16.to_le_bytes());
            bytes.extend(1u32.to_le_bytes());
            bytes.extend((12 + payload.len() as u32).to_le_bytes());
            bytes.extend(payload)
        }

        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        let mut image = Image::open(f.path()).unwrap();
        let mut out = vec![];
        image.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcd\0\0\0\x001234");
        assert_eq!(image.transfer_ranges(), [(0, 4), (8, 4)]);
        image.seek(SeekFrom::Start(9)).unwrap();
        let mut part = [0; 2];
        image.read_exact(&mut part).unwrap();
        assert_eq!(&part, b"23");
    }
}
