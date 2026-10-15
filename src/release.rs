//! This build's identity, embedded by `build.rs`. `ServerInfo` and `bbs version`
//! both report from here.

use serde::Serialize;

use crate::{config, db, proto, protocol};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REVISION: &str = env!("BBS_BUILD_REVISION");
pub const TARGET: &str = env!("BBS_BUILD_TARGET");
const DIRTY: Option<&str> = option_env!("BBS_BUILD_DIRTY");

/// Whether tracked files differed from the revision; unknown with the revision.
pub fn dirty() -> Option<bool> {
    DIRTY.and_then(|v| v.parse().ok())
}

/// Clean builds of a known revision are the only candidates for official
/// releases; dirty and unknown builds are development builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Clean,
    Dirty,
    Unknown,
}

impl Provenance {
    fn of(revision: &str, dirty: Option<bool>) -> Provenance {
        match (revision, dirty) {
            ("unknown", _) | (_, None) => Provenance::Unknown,
            (_, Some(true)) => Provenance::Dirty,
            (_, Some(false)) => Provenance::Clean,
        }
    }
}

pub fn info() -> proto::ReleaseInfo {
    proto::ReleaseInfo {
        version: VERSION.into(),
        revision: REVISION.into(),
        dirty: dirty(),
    }
}

#[cfg(test)]
#[path = "../build/identity.rs"]
mod build_identity;

#[derive(Serialize)]
struct Protocol {
    epoch: u32,
    capabilities: &'static [&'static str],
}

#[derive(Serialize)]
struct Report {
    version: &'static str,
    revision: &'static str,
    dirty: Option<bool>,
    provenance: Provenance,
    target: &'static str,
    protocol: Protocol,
    schema_version: usize,
    config_format: u32,
}

fn report() -> Report {
    Report {
        version: VERSION,
        revision: REVISION,
        dirty: dirty(),
        provenance: Provenance::of(REVISION, dirty()),
        target: TARGET,
        protocol: Protocol {
            epoch: protocol::EPOCH,
            capabilities: protocol::CAPABILITIES,
        },
        schema_version: db::SCHEMA_VERSION,
        config_format: config::FORMAT,
    }
}

/// `bbs version`: reads only compiled-in constants, so it opens no files,
/// configuration, database or connection.
pub fn print(json: bool) -> anyhow::Result<()> {
    let report = report();
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        let source = match (report.provenance, report.revision) {
            (_, "unknown") => "unknown revision".to_string(),
            (Provenance::Clean, revision) => revision.to_string(),
            (Provenance::Dirty, revision) => format!("{revision} with local changes"),
            (Provenance::Unknown, revision) => format!("{revision}, changes unknown"),
        };
        println!(
            "bbs {} ({source}, {})\nprotocol bbs.v{}: {}\nschema {}, config format {}",
            report.version,
            report.target,
            report.protocol.epoch,
            report.protocol.capabilities.join(", "),
            report.schema_version,
            report.config_format,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_requires_known_revision_and_dirty_state() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(Provenance::of(rev, Some(false)), Provenance::Clean);
        assert_eq!(Provenance::of(rev, Some(true)), Provenance::Dirty);
        assert_eq!(Provenance::of(rev, None), Provenance::Unknown);
        assert_eq!(Provenance::of("unknown", Some(false)), Provenance::Unknown);
    }

    #[test]
    fn report_uses_shared_constants() {
        let value = serde_json::to_value(report()).unwrap();
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["revision"], REVISION);
        assert_eq!(value["target"], TARGET);
        assert_eq!(value["protocol"]["epoch"], protocol::EPOCH);
        assert_eq!(
            value["protocol"]["capabilities"],
            serde_json::json!(protocol::CAPABILITIES)
        );
        assert_eq!(value["schema_version"], db::SCHEMA_VERSION);
        assert_eq!(value["config_format"], config::FORMAT);
        assert!(REVISION == "unknown" || REVISION.len() == 40);
    }

    #[test]
    fn build_identity_overrides_and_unknown_revisions() {
        let rev = "0123456789abcdef0123456789abcdef01234567";
        let git_rev = || Some(rev.to_string());
        let unreachable_rev = || -> Option<String> { panic!("git consulted") };
        let unreachable_dirty = || -> Option<bool> { panic!("git consulted") };
        // Overrides win without consulting git.
        assert_eq!(
            build_identity::resolve(Some(rev), Some("false"), unreachable_rev, unreachable_dirty),
            Ok((rev.into(), Some(false)))
        );
        // Unknown revisions never carry a dirty flag.
        assert_eq!(
            build_identity::resolve(
                Some("unknown"),
                Some("true"),
                unreachable_rev,
                unreachable_dirty
            ),
            Ok(("unknown".into(), None))
        );
        assert_eq!(
            build_identity::resolve(None, None, || None, unreachable_dirty),
            Ok(("unknown".into(), None))
        );
        assert_eq!(
            build_identity::resolve(None, None, || Some("abc123".into()), unreachable_dirty),
            Ok(("unknown".into(), None))
        );
        // Git supplies both when there is no override.
        assert_eq!(
            build_identity::resolve(None, None, git_rev, || Some(true)),
            Ok((rev.into(), Some(true)))
        );
        assert_eq!(
            build_identity::resolve(None, None, git_rev, || None),
            Ok((rev.into(), None))
        );
        for (revision, dirty) in [
            (Some("abc123"), None),
            (Some(&rev.to_uppercase()[..]), None),
            (Some(rev), Some("yes")),
            (None, Some("")),
        ] {
            assert!(build_identity::resolve(revision, dirty, git_rev, || Some(false)).is_err());
        }
    }

    #[test]
    fn server_info_reports_this_build() {
        let info = protocol::Policy::default().info(false);
        assert_eq!(info.release, Some(super::info()));
    }
}
