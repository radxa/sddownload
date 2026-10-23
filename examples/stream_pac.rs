//! Inspect a forward-only PAC stream and validate CRCs without USB access.
use anyhow::{Context, Result};
use sdimage::stream::PacStream;
use std::{
    fs::File,
    io::{self, Read},
};

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: stream_pac FILE.pac | -")?;
    let reader: Box<dyn Read> = if path == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(path)?)
    };
    let mut pac = PacStream::new(reader, true)?;
    println!("Product: {}, version: {}", pac.product, pac.version);
    while let Some(entry) = pac.next_entry()? {
        println!(
            "{}: offset {}, {} bytes",
            entry.partition, entry.offset, entry.size
        );
    }
    println!("Stream consumed; available PAC CRCs verified");
    Ok(())
}
