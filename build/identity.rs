//! Build identity rules, shared by `build.rs` and its unit tests.

/// The embedded revision (a full commit or `unknown`) and dirty flag. Explicit
/// overrides win; otherwise git is consulted only as needed. A dirty flag is
/// meaningless without the revision it is relative to, so it is dropped then.
pub fn resolve(
    revision_override: Option<&str>,
    dirty_override: Option<&str>,
    git_revision: impl FnOnce() -> Option<String>,
    git_dirty: impl FnOnce() -> Option<bool>,
) -> Result<(String, Option<bool>), String> {
    let revision = match revision_override {
        Some(r) if r == "unknown" || is_full_revision(r) => r.to_string(),
        Some(r) => {
            return Err(format!(
                "BBS_BUILD_REVISION must be a full 40-character commit or `unknown`, not {r:?}"
            ));
        }
        None => git_revision()
            .filter(|r| is_full_revision(r))
            .unwrap_or_else(|| "unknown".into()),
    };
    let dirty = match dirty_override {
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(v) => return Err(format!("BBS_BUILD_DIRTY must be true or false, not {v:?}")),
        None if revision != "unknown" => git_dirty(),
        None => None,
    };
    Ok((revision.clone(), dirty.filter(|_| revision != "unknown")))
}

fn is_full_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
