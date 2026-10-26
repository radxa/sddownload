//! Inspection is dispatched before download planning, so a PAC listing cannot
//! initialize USB, send loaders, flash, erase, read back, or reboot a device.
use super::{listing_table::write_table, streaming, with_fastboot, Cli};
use crate::{config::Config, plan};
use anyhow::{ensure, Context, Result};
use sdimage::{
    pac::PacFileType,
    stream::{PacStream, Resource},
};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

#[derive(Debug)]
struct PacListing {
    source: String,
    storage: String,
    rows: Vec<Vec<String>>,
}

fn resource_row(storage: &str, resource: Resource) -> Vec<String> {
    let role = if resource.container {
        "PAC container"
    } else {
        match resource.kind {
            PacFileType::Fda => "FDA (bootstrap; not a partition)",
            PacFileType::OspiDa | PacFileType::Dloader => {
                "download agent (bootstrap; not a partition)"
            }
            _ => "image",
        }
    };
    let notes = if resource.size == 0 {
        format!("{role}; empty entry")
    } else {
        role.into()
    };
    vec![
        plan::storage_partition(storage, &resource.name),
        resource.size.to_string(),
        resource.kind.to_string(),
        notes,
    ]
}

fn pac_rows(options: &Cli) -> Result<Vec<PacListing>> {
    options.validate_stdin()?;
    let mut listings = vec![];
    for (storage, path) in options.inputs() {
        eprintln!("Inspecting PAC: {}", path.display());
        let source = path.display().to_string();
        let resources = if path == Path::new("-") {
            read_resources(
                PacStream::with_resource_metadata(streaming::input(path)?, options.verify_first)?,
                storage,
            )
        } else {
            read_resources(
                PacStream::with_seekable_resource_metadata(
                    File::open(path).with_context(|| format!("open PAC {}", path.display()))?,
                    options.verify_first,
                )?,
                storage,
            )
        }
        .with_context(|| format!("inspect PAC {}", path.display()))?;
        // Preserve repeated resources too: this is a catalog of the PAC's
        // table entries, not a deduplicated or inferred download plan.
        let mut rows: Vec<_> = resources
            .into_iter()
            .map(|resource| resource_row(storage, resource))
            .collect();
        rows.sort_by(|left, right| left[0].cmp(&right[0]));
        listings.push(PacListing {
            source,
            storage: storage.into(),
            rows,
        });
    }
    Ok(listings)
}

fn read_resources<R: Read>(mut pac: PacStream<R>, storage: &str) -> Result<Vec<Resource>> {
    ensure!(
        pac.version == u32::from(storage == "pac"),
        "--pac expects version 1; storage-specific PAC arguments expect version 0"
    );
    pac.finish()?;
    Ok(pac.resources)
}

fn print_pac(options: &Cli, output: &mut impl Write) -> Result<()> {
    // Finish traversal (and CRC checks when requested) before publishing tables.
    let listings = pac_rows(options)?;
    let mut count = 0;
    for (index, listing) in listings.iter().enumerate() {
        if index > 0 {
            writeln!(output)?;
        }
        writeln!(
            output,
            "PAC: {} (input --{})",
            listing.source, listing.storage
        )?;
        write_table(
            output,
            &["RESOURCE", "IMAGE_BYTES", "KIND", "NOTES"],
            &listing.rows,
        )?;
        count += listing.rows.len();
    }
    let verification = if options.verify_first {
        "PAC CRCs checked"
    } else {
        "metadata only; CRCs not checked (use --verify-first for full verification)"
    };
    eprintln!("PAC listing: {count} resources; {verification}. Names come from PAC tables; no slot names are inferred. IMAGE_BYTES is the PAC payload size, not device partition capacity. FDA/download agents and PAC containers are not ordinary flash/read partitions.");
    output.flush()?;
    Ok(())
}

pub(super) fn run(options: &Cli) -> Result<()> {
    ensure!(options.flash.is_empty() && options.read.is_empty() && options.erase.is_empty()
        && !options.reboot && options.unpac_path.is_none(), "--list-partitions cannot be combined with --flash, --read, --erase, --reboot, or --unpac-path");
    if !options.inputs().is_empty() {
        return print_pac(options, &mut io::stdout().lock());
    }
    options.validate_stdin()?;
    let settings = Config::load(options.config.as_deref())?;
    let directory = tempfile::Builder::new()
        .prefix("sddownload-list-")
        .tempdir()?;
    if options.debug {
        let path = directory.keep();
        eprintln!("Debug images retained in {}", path.display());
        streaming::enter_fastboot(options, &settings, &path)?;
    } else {
        streaming::enter_fastboot(options, &settings, directory.path())?;
    }
    let listing = with_fastboot(options, settings.timeout("get_stage"), |client| {
        client.list_partitions()
    })?;
    if let Some(warning) = listing.warning {
        eprintln!("Partial fastboot partition listing: {warning}");
    }
    let mut output = io::stdout().lock();
    let rows: Vec<_> = listing
        .partitions
        .into_iter()
        .map(|partition| {
            vec![
                partition.name,
                partition
                    .size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "-".into()),
                partition.kind.unwrap_or_else(|| "-".into()),
                partition
                    .logical
                    .map(|value| if value { "yes" } else { "no" })
                    .unwrap_or("-")
                    .into(),
            ]
        })
        .collect();
    write_table(
        &mut output,
        &["PARTITION", "SIZE_BYTES", "TYPE", "LOGICAL"],
        &rows,
    )?;
    eprintln!("Fastboot listing contains only device-advertised names and may be incomplete; '-' means unavailable. Use the exact PARTITION name with --read or --flash.");
    output.flush()?;
    Ok(())
}
