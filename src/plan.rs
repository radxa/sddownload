use crate::{cli::Cli, config::Config};
use anyhow::{ensure, Context, Result};
use sdimage::pac::{Pac, PacFileType};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Flash {
    pub partition: String,
    pub path: PathBuf,
}
#[derive(Debug, Default)]
pub struct Plan {
    pub fda: Option<PathBuf>,
    pub images: Vec<Flash>,
    pub product: String,
    pub auto_erase: Vec<String>,
}

pub fn storage_partition(storage: &str, partition: &str) -> String {
    if storage == "pac" {
        partition.into()
    } else {
        format!(
            "{}{}{partition}",
            if storage == "sd" { "sd3" } else { storage },
            if partition.contains('$') { "$" } else { "$$" }
        )
    }
}

impl Plan {
    pub fn prepare(options: &Cli, settings: &Config, temporary: &Path) -> Result<Self> {
        let mut plan = Self::default();
        let mut ospi_da = None;
        let mut emmc_da = None;
        let mut ssb_fda = None;
        let mut ssb_da = None;

        for (storage, path) in options.inputs() {
            sdeasyboot::statusln!("Opening PAC: {}", path.display());
            let pac = Pac::open(path, !options.skip_verify)
                .with_context(|| format!("load {storage} PAC {}", path.display()))?;

            ensure!(
                pac.version == u32::from(storage == "pac"),
                "--pac expects version 1; storage-specific PAC arguments expect version 0"
            );

            if plan.product.is_empty() {
                plan.product = pac.product.clone();
            }

            let output = if let Some(output) = &options.unpac_path {
                output.clone()
            } else {
                temporary.join(storage)
            };
            fs::create_dir_all(&output)?;

            for name in &pac.empty {
                if matches!(storage, "emmc1" | "sd" | "pac") {
                    let name = storage_partition(storage, name);

                    if settings.auto_erase.iter().any(|part| name.contains(part)) {
                        plan.auto_erase.push(name);
                    }
                }
            }

            sdeasyboot::statusln!("Extracting PAC to: {}", output.display());
            let mut current = 0;
            let total = pac.entries.len();
            let extracted = pac.extract_with_progress(&output, |entry, destination| {
                current += 1;
                sdeasyboot::statusln!(
                    "[{current}/{total}] Extracting {} ({} bytes) -> {}",
                    entry.partition,
                    entry.size,
                    destination.display()
                );
            })?;
            sdeasyboot::statusln!(
                "Extracted {} files from {}",
                extracted.len(),
                path.display()
            );

            for (entry, path) in extracted {
                let flash = Flash {
                    partition: storage_partition(storage, &entry.partition),
                    path: path.clone(),
                };

                match entry.kind {
                    PacFileType::Fda => {
                        if entry.partition.contains("ssb") {
                            ssb_fda = Some(Flash {
                                partition: "ssb_fda".into(),
                                path,
                            })
                        } else {
                            plan.fda = Some(path)
                        }
                    }
                    PacFileType::OspiDa => {
                        if !entry.partition.contains("ssb") {
                            ospi_da = Some(Flash {
                                partition: "ospi1$da".into(),
                                path,
                            })
                        }
                    }
                    PacFileType::Dloader => {
                        if entry.partition.contains("ssb") {
                            ssb_da = Some(Flash {
                                partition: "ssb_emmc1$da".into(),
                                path,
                            })
                        } else {
                            emmc_da = Some(Flash {
                                partition: "emmc1$da".into(),
                                path,
                            })
                        }
                    }
                    PacFileType::Pac => {}
                    _ => {
                        if options.b_part && entry.slot_ab {
                            plan.images.push(Flash {
                                partition: format!("{}_a", flash.partition),
                                path: flash.path.clone(),
                            });
                            plan.images.push(Flash {
                                partition: format!("{}_b", flash.partition),
                                path: flash.path,
                            });
                        } else {
                            plan.images.push(flash)
                        }
                    }
                }
            }
        }

        let mut loaders: Vec<_> = [ospi_da, emmc_da, ssb_fda, ssb_da]
            .into_iter()
            .flatten()
            .collect();
        loaders.append(&mut plan.images);
        plan.images = loaders;

        Ok(plan)
    }
}

pub fn is_da(name: &str) -> bool {
    matches!(name, "ospi1$da" | "emmc1$da")
}

pub fn is_ssb(name: &str) -> bool {
    matches!(name, "ssb_fda" | "ssb_emmc1$da")
}

pub fn is_boot(name: &str) -> bool {
    matches!(
        name,
        "sd3$$boot0" | "sd3$$boot1" | "ssb_sd3$$boot0" | "ssb_sd3$$boot1"
    )
}
