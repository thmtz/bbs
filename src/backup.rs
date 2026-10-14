//! Local administrative backup; never selects a remote backend or migrates state.

use std::path::Path;

use anyhow::{Context, Result, bail};

pub fn run(state_dir: &Path, destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        bail!(
            "backup destination already exists: {}",
            destination.display()
        );
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = destination.as_os_str().to_os_string();
        sidecar.push(suffix);
        if Path::new(&sidecar).symlink_metadata().is_ok() {
            bail!("backup destination has a SQLite sidecar; choose a new path");
        }
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Keep incomplete snapshots under a temporary name. NamedTempFile creates
    // private files (0600 on Unix); backups contain message and credential data.
    let temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating backup in {}", parent.display()))?;
    crate::db::backup(&state_dir.join("bbs.db"), temporary.path())?;
    temporary.as_file().sync_all().context("syncing backup")?;
    temporary.persist_noclobber(destination).with_context(|| {
        format!(
            "saving backup to {} without overwriting",
            destination.display()
        )
    })?;
    Ok(())
}
