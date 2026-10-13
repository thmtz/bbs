use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::backend::{Backend, Local, Remote};
use crate::cli::{Cli, Command, GroupCommand, HumanCommand};
use crate::config::Config;
use crate::proto;
use crate::service::{Caller, MAX_BODY_BYTES, fmt_time};

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

pub(crate) fn state_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/state/bbs"))
}

/// Stored timestamps are fixed-width; show them to second precision.
fn display_time(stored: &str) -> String {
    format!("{}Z", &stored[..19.min(stored.len())])
}

pub async fn run(cli: Cli) -> Result<()> {
    let dir = state_dir()?;
    match &cli.command {
        Command::Init { server, human } => {
            return init(&dir, server, human, cli.as_human.as_deref()).await;
        }
        Command::Hook { harness } => return crate::hook::run(*harness, &dir).await,
        Command::Serve { listen, state_dir } => {
            return crate::serve::run(*listen, state_dir.as_deref().unwrap_or(&dir)).await;
        }
        _ => {}
    }
    let config = Config::load()?;
    let caller = Caller {
        token: cli.token.clone(),
        human: config.human(cli.as_human.as_deref()),
    };
    let mut backend = Backend::open(&config, &dir, caller).await?;
    match cli.command {
        Command::View => crate::view::run(&mut backend).await,
        Command::Read {
            limit, wait: true, ..
        } => {
            print!("{}", format_messages(&backend.wait_for_mail(limit).await?)?);
            Ok(())
        }
        command => {
            print!("{}", dispatch(&mut backend, command).await?);
            Ok(())
        }
    }
}

async fn dispatch(backend: &mut Backend, command: Command) -> Result<String> {
    Ok(match command {
        Command::Init { .. } | Command::Hook { .. } | Command::Serve { .. } | Command::View => {
            unreachable!()
        }
        Command::Human {
            command: HumanCommand::Add { name },
        } => {
            backend
                .add_human(proto::AddHumanRequest { name: name.clone() })
                .await?;
            format!("Added human {name}.\n")
        }
        Command::Group { command } => match command {
            GroupCommand::Register { group } => {
                let r = backend
                    .group_register(proto::GroupRequest { group })
                    .await?;
                format!("Registered group @{}.\n", r.group)
            }
            GroupCommand::Join { group } => {
                let r = backend.group_join(proto::GroupRequest { group }).await?;
                if r.joined {
                    format!("Joined @{}.\n", r.group)
                } else {
                    format!("Already in @{}.\n", r.group)
                }
            }
            GroupCommand::Leave { group } => {
                let r = backend.group_leave(proto::GroupRequest { group }).await?;
                format!("Left @{}.\n", r.group)
            }
            GroupCommand::Remove { group, agent } => {
                let req = proto::GroupRemoveRequest { group, agent };
                let r = backend.group_remove(req).await?;
                format!("Removed {} from @{}.\n", r.agent, r.group)
            }
            GroupCommand::List => format_groups(&backend.group_list().await?.groups)?,
        },
        Command::Register { note } => {
            let req = proto::RegisterRequest {
                note,
                pwd: std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            };
            let r = backend.register(req).await?;
            format!("Welcome to the BBS. {}\n", identity_line(&r.name, &r.token))
        }
        Command::Status { text } => {
            backend.set_status(proto::SetStatusRequest { text }).await?;
            "Status updated.\n".to_string()
        }
        Command::Bye => format!("Goodbye, {}.\n", backend.bye().await?.name),
        Command::Send { to, body } => {
            let req = proto::SendRequest {
                to,
                body: read_body(body)?,
            };
            let r = backend.send(req).await?;
            match r.recipients {
                Some(n) => format!("Sent to {} ({n} recipients).\n", r.to),
                None => format!("Sent to {}.\n", r.to),
            }
        }
        Command::Read {
            limit,
            archived,
            since,
            ..
        } => {
            let req = proto::ReadRequest {
                limit,
                archived,
                since: since.map(|s| parse_since(&s, Utc::now())).transpose()?,
                from_kind: None,
            };
            format_messages(&backend.read(req).await?.messages)?
        }
        Command::Whoami => format!("{}\n", backend.whoami().await?.name),
        Command::Agents { all } => {
            let r = backend
                .list_agents(proto::ListAgentsRequest { all })
                .await?;
            format_agents(&r)
        }
    })
}

/// Merge `--server`/`--human` into the config file, then reserve the human
/// name on the server, or in a newly created local database.
async fn init(
    dir: &Path,
    server: &Option<String>,
    human: &Option<String>,
    as_human: Option<&str>,
) -> Result<()> {
    if server.is_some() || human.is_some() {
        let mut file = Config::read()?;
        file.server = server.clone().or(file.server);
        file.human = human.clone().or(file.human);
        file.write()?;
    }
    let config = Config::load()?;
    let name = human
        .clone()
        .or_else(|| config.human(as_human))
        .context("$USER is not set; pass --human <name>")?;
    let (mut backend, done) = match &config.server {
        Some(addr) => (
            Backend::Remote(Remote::connect(addr, Caller::default()).await?),
            format!("BBS configured for server {addr}"),
        ),
        None => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            (
                Backend::Local(Local::open(dir, Caller::default())?),
                format!("BBS initialized at {}", dir.display()),
            )
        }
    };
    let req = proto::EnsureHumanRequest { name: name.clone() };
    backend.ensure_human(req).await?;
    println!("{done}\nHuman: {name}");
    Ok(())
}

fn format_groups(groups: &[proto::Group]) -> Result<String> {
    let mut out = String::new();
    for g in groups {
        let members = if g.members.is_empty() {
            "(no agents)".to_string()
        } else {
            g.members.join(", ")
        };
        writeln!(out, "@{}: {members}", g.name)?;
    }
    Ok(out)
}

pub(crate) fn identity_line(name: &str, token: &str) -> String {
    format!("Your name is \"{name}\", access BBS with `bbs --token {token} ...`")
}

/// The message body from the argument, or from stdin when omitted or "-".
/// Read one byte past the limit so the service can reject oversized bodies.
fn read_body(body: Option<String>) -> Result<String> {
    match body {
        Some(b) if b != "-" => Ok(b),
        _ => {
            let mut s = String::new();
            std::io::stdin()
                .take(MAX_BODY_BYTES as u64 + 1)
                .read_to_string(&mut s)
                .context("reading message body from stdin")?;
            Ok(s)
        }
    }
}

/// Resolve `--since` (RFC3339 or a duration like 2h) to RFC3339.
fn parse_since(s: &str, now: DateTime<Utc>) -> Result<String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(fmt_time(t.with_timezone(&Utc)));
    }
    let d = humantime::parse_duration(s).with_context(|| {
        format!("invalid --since {s:?}: expected RFC3339 or a duration like 2h")
    })?;
    Ok(fmt_time(now - chrono::Duration::from_std(d)?))
}

pub(crate) fn format_messages(msgs: &[proto::Message]) -> Result<String> {
    let mut out = String::new();
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let human = if m.from_kind() == proto::Kind::Human {
            " (human)"
        } else {
            ""
        };
        let to = match (&m.to_group, &m.to_name) {
            (Some(g), _) => format!("@{g}"),
            (None, to) => to.clone().unwrap_or_default(),
        };
        writeln!(
            out,
            "[{}] from {}{human} to {to}:",
            display_time(&m.created_at),
            m.from_name
        )?;
        out.push_str(m.body.trim_end_matches('\n'));
        out.push('\n');
    }
    Ok(out)
}

fn format_agents(r: &proto::ListAgentsResponse) -> String {
    let mut table: Vec<[String; 6]> = vec![[
        "NAME".into(),
        "KIND".into(),
        "GROUPS".into(),
        "ACTIVE_AT".into(),
        "WHERE".into(),
        "STATUS".into(),
    ]];
    for h in &r.humans {
        table.push([
            h.name.clone(),
            "human".into(),
            "*".into(),
            display_time(&h.active_at),
            String::new(),
            String::new(),
        ]);
    }
    for a in &r.agents {
        let kind = if a.retired { "retired" } else { "agent" };
        let g = a.groups.join(",");
        table.push([
            a.name.clone(),
            kind.into(),
            if g.is_empty() { "-".into() } else { g },
            display_time(&a.active_at),
            format!("{}:{}", a.hostname, a.pwd),
            a.status.clone().unwrap_or_default(),
        ]);
    }
    render_table(&table)
}

fn render_table(rows: &[[String; 6]]) -> String {
    let mut widths: HashMap<usize, usize> = HashMap::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let w = widths.entry(i).or_default();
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<w$}  ", w = widths[&i]);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
