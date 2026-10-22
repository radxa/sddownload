use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

#[derive(Debug)]
pub struct Config {
    pub timeout: BTreeMap<String, u64>,
    pub auto_erase: Vec<String>,
}
#[derive(Default, Deserialize)]
struct Overrides {
    #[serde(default)]
    timeout: BTreeMap<String, u64>,
    auto_erase: Option<Erase>,
}
#[derive(Deserialize)]
struct Erase {
    emmc: Vec<String>,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut timeout: BTreeMap<String, u64> = [
            ("detect_dev", 60),
            ("get_stage", 20),
            ("ospi1$da", 20),
            ("emmc1$da", 20),
            ("ospi1$$sfs", 20),
            ("ospi2$$sfs", 20),
            ("default", 400),
        ]
        .into_iter()
        .map(|(s, v)| (s.into(), v))
        .collect();
        let mut auto_erase = vec!["exception_log".into(), "misc".into()];

        if let Some(path) = path {
            let settings: Overrides = serde_json::from_slice(
                &fs::read(path).with_context(|| format!("read config {}", path.display()))?,
            )
            .context("invalid application JSON config")?;
            timeout.extend(settings.timeout);

            if let Some(erase) = settings.auto_erase {
                auto_erase = erase.emmc;
            }
        }

        Ok(Self {
            timeout,
            auto_erase,
        })
    }

    pub fn timeout(&self, key: &str) -> Duration {
        Duration::from_secs(*self.timeout.get(key).unwrap_or(&self.timeout["default"]))
    }
}
