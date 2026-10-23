pub mod elf;
pub mod pac;
pub mod sparse;
pub mod stream;

use anyhow::Result;
use md5::{Digest, Md5};
use std::io::Read;

pub const BZIP2: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);
pub fn md5_reader(mut reader: impl Read) -> Result<String> {
    let mut digest = Md5::new();
    let mut buffer = vec![0; 1024 * 1024];

    loop {
        let count = reader.read(&mut buffer)?;

        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }

    Ok(format!("{:x}", digest.finalize()))
}
