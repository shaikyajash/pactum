//! Crash-safe JSON persistence for agent state.
//!
//! A resolver must remember its open escrows across restarts, and a maker must
//! remember its secrets: losing them mid-swap would leave escrows to time out.
//! Writes go to a temp file that is fsynced and renamed into place, so a crash
//! leaves either the old or the new state, never a torn file. Files are created
//! owner-only because maker state contains unrevealed secrets.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Serialize, de::DeserializeOwned};

pub struct JsonStore<T> {
    path: PathBuf,
    _t: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> JsonStore<T> {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), _t: PhantomData }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> anyhow::Result<Option<T>> {
        match fs::read(&self.path) {
            Ok(raw) => Ok(Some(
                serde_json::from_slice(&raw)
                    .with_context(|| format!("parsing {}", self.path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    pub fn save(&self, value: &T) -> anyhow::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_handles_missing_file() {
        let dir = std::env::temp_dir().join(format!("pactum-store-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = JsonStore::<Vec<u32>>::new(dir.join("state.json"));

        assert_eq!(store.load().unwrap(), None);
        store.save(&vec![1, 2, 3]).unwrap();
        assert_eq!(store.load().unwrap(), Some(vec![1, 2, 3]));
        store.save(&vec![4]).unwrap();
        assert_eq!(store.load().unwrap(), Some(vec![4]));
        assert!(!store.path().with_extension("tmp").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "state may hold secrets");
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
