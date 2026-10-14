//! Local account administration, authorized by access to the server state.

use crate::{
    cli::{AdminCommand, AdminHumanCommand},
    db, service,
};
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(dir: &Path, command: &AdminCommand) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut conn = db::open(&dir.join("bbs.db"))?;
    db::migrate(&mut conn)?;
    match command {
        AdminCommand::Human { command } => match command {
            AdminHumanCommand::Add { name } => {
                println!("{}", service::admin_issue_token(&mut conn, name, true)?)
            }
            AdminHumanCommand::Rotate { name } => {
                println!("{}", service::admin_issue_token(&mut conn, name, false)?)
            }
            AdminHumanCommand::Revoke { name } => {
                service::admin_revoke_token(&mut conn, name)?;
                println!("Revoked credentials for {name}.");
            }
        },
    }
    Ok(())
}
