//! Seekable and forward-only expanded views of Android sparse images. Data is expanded into
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

/// Sequential expanded sparse segments. The underlying input never seeks.
pub struct StreamImage<R> {
    reader: R,
    encoded: u64,
    prefix: io::Cursor<Vec<u8>>,
    pub size: u64,
    pub sparse: bool,
    chunks: u32,
    header_size: u64,
    block: u64,
    logical: u64,
    remaining: u64,
    contents: Contents,
    fill_position: u64,
    raw_pending: bool,
}

impl<R: Read> StreamImage<R> {
    pub fn new(mut reader: R, encoded_size: u64) -> Result<Self> {
        let mut magic = vec![0; encoded_size.min(4) as usize];
        reader.read_exact(&mut magic)?;
        let sparse = magic.len() == 4 && u32le(&magic, 0) == 0xed26ff3a;
        let mut image = Self {
            reader,
            encoded: encoded_size - magic.len() as u64,
            prefix: io::Cursor::new(magic),
            size: encoded_size,
            sparse,
            chunks: 0,
            header_size: 0,
            block: 0,
            logical: 0,
            remaining: 0,
            contents: Contents::Raw(0),
            fill_position: 0,
            raw_pending: !sparse,
        };
        if sparse {
            let mut header = [0; 28];
            header[..4].copy_from_slice(image.prefix.get_ref());
            image.exact(&mut header[4..])?;
            let file_header = u16le(&header, 8) as u64;
            image.header_size = u16le(&header, 10) as u64;
            image.block = u32le(&header, 12) as u64;
            image.size = image
                .block
                .checked_mul(u32le(&header, 16) as u64)
                .context("sparse image overflow")?;
            image.chunks = u32le(&header, 20);
            ensure!(
                u16le(&header, 4) == 1
                    && file_header >= 28
                    && image.header_size >= 12
                    && image.block > 0
                    && image.block.is_multiple_of(4),
                "invalid sparse geometry"
            );
            ensure!(file_header <= encoded_size, "sparse header exceeds image");
            image.skip(file_header - 28)?;
            ensure!(
                u64::from(image.chunks) <= image.encoded / image.header_size,
                "sparse chunk count exceeds image"
            );
        }
        Ok(image)
    }

    fn exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        ensure!(
            bytes.len() as u64 <= self.encoded,
            "sparse data exceeds image bounds"
        );
        self.reader
            .read_exact(bytes)
            .context("truncated sparse image")?;
        self.encoded -= bytes.len() as u64;
        Ok(())
    }

    fn skip(&mut self, mut bytes: u64) -> Result<()> {
        let mut buffer = [0; 4096];
        while bytes != 0 {
            let count = bytes.min(buffer.len() as u64) as usize;
            self.exact(&mut buffer[..count])?;
            bytes -= count as u64;
        }
        Ok(())
    }

    /// Each segment is read through `Read` before requesting the next segment.
    /// Unread data is drained automatically. Interior holes produce no segment.
    pub fn next_segment(&mut self) -> Result<Option<(u64, u64)>> {
        io::copy(&mut *self, &mut io::sink())?;
        if self.raw_pending {
            self.raw_pending = false;
            self.remaining = self.size;
            return Ok((self.size > 0).then_some((0, self.size)));
        }
        while self.chunks > 0 {
            let mut header = [0; 12];
            self.exact(&mut header)?;
            self.skip(self.header_size - 12)?;
            self.chunks -= 1;
            let expanded = self
                .block
                .checked_mul(u32le(&header, 4) as u64)
                .context("sparse chunk overflow")?;
            let total = u32le(&header, 8) as u64;
            ensure!(
                total >= self.header_size && total - self.header_size <= self.encoded,
                "sparse chunk exceeds image"
            );
            let payload = total - self.header_size;
            let start = self.logical;
            self.logical = self
                .logical
                .checked_add(expanded)
                .context("sparse size overflow")?;
            ensure!(
                self.logical <= self.size,
                "sparse chunk exceeds expanded size"
            );
            self.contents = match u16le(&header, 0) {
                0xcac1 => {
                    ensure!(payload == expanded, "RAW chunk length mismatch");
                    Contents::Raw(0)
                }
                0xcac2 => {
                    ensure!(payload == 4, "FILL chunk length mismatch");
                    let mut fill = [0; 4];
                    self.exact(&mut fill)?;
                    Contents::Fill(fill)
                }
                0xcac3 => {
                    ensure!(payload == 0, "DONT_CARE payload is not empty");
                    if self.logical != self.size {
                        continue;
                    }
                    Contents::Zero
                }
                0xcac4 => {
                    ensure!(payload == 4 && expanded == 0, "invalid sparse CRC chunk");
                    self.skip(4)?;
                    continue;
                }
                kind => anyhow::bail!("unknown sparse chunk {kind:#x}"),
            };
            if expanded == 0 {
                continue;
            }
            self.remaining = expanded;
            self.fill_position = 0;
            return Ok(Some((start, expanded)));
        }
        ensure!(
            !self.sparse || self.logical == self.size,
            "sparse expanded size mismatch"
        );
        ensure!(self.encoded == 0, "unexpected trailing image data");
        Ok(None)
    }

    pub fn finish(&mut self) -> Result<()> {
        while self.next_segment()?.is_some() {}
        Ok(())
    }
}

impl<R: Read> Read for StreamImage<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.remaining.min(buffer.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let buffer = &mut buffer[..count];
        let count = match self.contents {
            Contents::Raw(_) => {
                let prefix = if self.sparse {
                    0
                } else {
                    self.prefix.read(buffer)?
                };
                if prefix > 0 {
                    prefix
                } else {
                    let count = self.reader.read(buffer)?;
                    if count == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated image",
                        ));
                    }
                    self.encoded = self
                        .encoded
                        .checked_sub(count as u64)
                        .ok_or_else(|| io::Error::other("image exceeded encoded size"))?;
                    count
                }
            }
            Contents::Fill(fill) => {
                for (index, byte) in buffer.iter_mut().enumerate() {
                    *byte = fill[((self.fill_position + index as u64) % 4) as usize];
                }
                self.fill_position += count as u64;
                count
            }
            Contents::Zero => {
                buffer.fill(0);
                count
            }
        };
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_stream_retains_short_prefixes_and_rejects_truncation() {
        for bytes in [b"abc".as_slice(), b"long raw image".as_slice()] {
            let mut image = StreamImage::new(bytes, bytes.len() as u64).unwrap();
            assert_eq!(image.next_segment().unwrap(), Some((0, bytes.len() as u64)));
            let mut output = vec![];
            image.read_to_end(&mut output).unwrap();
            assert_eq!(output, bytes);
            image.finish().unwrap();
        }
        let mut image = StreamImage::new(b"abcd".as_slice(), 8).unwrap();
        assert!(image.finish().is_err());
    }

    #[test]
    fn sparse_stream_has_forward_data_fill_and_trailing_zero_segments() {
        let mut bytes = vec![];
        bytes.extend(0xed26ff3au32.to_le_bytes());
        for value in [1u16, 0, 28, 12] {
            bytes.extend(value.to_le_bytes());
        }
        for value in [4u32, 4, 4, 0] {
            bytes.extend(value.to_le_bytes());
        }
        for (kind, payload) in [
            (0xcac1u16, b"abcd".as_slice()),
            (0xcac3, b"".as_slice()),
            (0xcac2, b"1234".as_slice()),
            (0xcac3, b"".as_slice()),
        ] {
            bytes.extend(kind.to_le_bytes());
            bytes.extend(0u16.to_le_bytes());
            bytes.extend(1u32.to_le_bytes());
            bytes.extend((12 + payload.len() as u32).to_le_bytes());
            bytes.extend(payload);
        }
        let mut image = StreamImage::new(bytes.as_slice(), bytes.len() as u64).unwrap();
        let mut segments = vec![];
        while let Some((offset, size)) = image.next_segment().unwrap() {
            let mut data = vec![];
            image.read_to_end(&mut data).unwrap();
            assert_eq!(data.len() as u64, size);
            segments.push((offset, data));
        }
        assert_eq!(
            segments,
            [
                (0, b"abcd".to_vec()),
                (8, b"1234".to_vec()),
                (12, vec![0; 4])
            ]
        );
        bytes.pop();
        let mut image = StreamImage::new(bytes.as_slice(), bytes.len() as u64).unwrap();
        assert!(image.finish().is_err());
    }
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
