use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::ChainConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub chains: Vec<ChainConfig>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(!cfg.chains.is_empty(), "{}: no chains configured", path.display());
        Ok(cfg)
    }

    pub fn chain(&self, name_or_id: &str) -> anyhow::Result<&ChainConfig> {
        self.chains
            .iter()
            .find(|c| c.name == name_or_id || c.id.to_string() == name_or_id)
            .with_context(|| format!("unknown chain {name_or_id}"))
    }
}
