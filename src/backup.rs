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
    let source = state_dir.join("bbs.db");
    let resolved_source = source
        .canonicalize()
        .with_context(|| format!("opening backup source {}", source.display()))?;
    let resolved_destination = parent.canonicalize()?.join(
        destination
            .file_name()
            .context("backup destination needs a filename")?,
    );
    // SQLite owns its sidecar names even when those files do not currently
    // exist. Check both the supplied path and a symlink's resolved target.
    for source_path in [resolved_source, state_dir.canonicalize()?.join("bbs.db")] {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = source_path.as_os_str().to_os_string();
            sidecar.push(suffix);
            if resolved_destination == Path::new(&sidecar) {
                bail!("backup destination is a source database sidecar; choose a different path");
            }
        }
    }
    // Keep incomplete snapshots under a temporary name. NamedTempFile creates
    // private files (0600 on Unix); backups contain message and credential data.
    let temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating backup in {}", parent.display()))?;
    crate::db::backup(&source, temporary.path())?;
    temporary.as_file().sync_all().context("syncing backup")?;
    temporary.persist_noclobber(destination).with_context(|| {
        format!(
            "saving backup to {} without overwriting",
            destination.display()
        )
    })?;
    Ok(())
}
