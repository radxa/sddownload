//! Metadata inspection through the reusable PAC library, with no extraction
//! and no USB device access.
use anyhow::{Context, Result};
use sdimage::pac::Pac;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: inspect_pac FILE.pac")?;
    let pac = Pac::open(path, true)?;

    println!("Product: {}", pac.product);
    println!("Version: {}", pac.version);

    for entry in pac.entries {
        println!(
            "{}: {} bytes, type {}, A/B={}",
            entry.partition, entry.size, entry.kind, entry.slot_ab
        );
    }

    Ok(())
}
