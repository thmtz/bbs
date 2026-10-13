use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::cli::{Cli, Command, GroupCommand, HumanCommand};
use crate::db::{self, Kind};
use crate::proto;
use crate::service::{self, Caller, MAX_BODY_BYTES, fmt_time};

const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const WAIT_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);

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

pub fn run(cli: Cli) -> Result<()> {
    let dir = state_dir()?;
    let db_path = dir.join("bbs.db");
    match &cli.command {
        Command::Init { human } => return init(&dir, &db_path, human.as_deref()),
        Command::Hook { harness } => return crate::hook::run(*harness, &db_path),
        Command::Serve { listen, state_dir } => {
            return crate::serve::run(*listen, state_dir.as_deref().unwrap_or(&dir));
        }
        _ => {}
    }
    if !db_path.exists() {
        bail!("BBS is not initialized; run `bbs init`");
    }
    let mut conn = db::open(&db_path)?;
    db::migrate(&mut conn)?;
    let caller = Caller {
        token: cli.token.clone(),
        human: cli.as_human.clone().or_else(|| std::env::var("USER").ok()),
    };
    match cli.command {
        Command::View => crate::view::run(&mut conn, &caller),
        Command::Read {
            limit, wait: true, ..
        } => read_wait(&mut conn, &caller, &dir, limit),
        command => {
            print!("{}", dispatch(&mut conn, &caller, command)?);
            Ok(())
        }
    }
}

fn read_wait(conn: &mut Connection, caller: &Caller, dir: &Path, limit: Option<u32>) -> Result<()> {
    let me = service::whoami(conn, caller)?;
    let me = db::Principal {
        kind: match me.kind() {
            proto::Kind::Agent => Kind::Agent,
            _ => Kind::Human,
        },
        id: me.id,
        name: me.name,
    };
    let _lock = lock_waiter(&me, dir)?;
    loop {
        let req = proto::ReadRequest {
            limit,
            ..Default::default()
        };
        let msgs = service::read(conn, caller, req)?.messages;
        if !msgs.is_empty() {
            print!("{}", format_messages(&msgs)?);
            return Ok(());
        }
        // Another reader of the same inbox may win the race; keep waiting until we get mail.
        wait_for_unread(conn, caller, &me)?;
    }
}

/// Allow one `read --wait` per inbox, so waiters never race for the same mail.
/// The advisory lock lives beside the database; the OS releases it when the
/// process exits, so a leftover lock file is harmless.
fn lock_waiter(me: &db::Principal, dir: &Path) -> Result<std::fs::File> {
    let locks = dir.join("locks");
    std::fs::create_dir_all(&locks).with_context(|| format!("creating {}", locks.display()))?;
    let path = locks.join(format!("{}-{}.lock", me.kind.as_str(), me.id));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!(
                "another `bbs read --wait` is already running for {}",
                me.name
            )
        }
        Err(std::fs::TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// Block until the caller has unread mail. Never holds a transaction while idle:
/// `PRAGMA data_version` changes only when another connection commits, so we
/// poll it once a second and run the unread query only after a change.
fn wait_for_unread(conn: &mut Connection, caller: &Caller, me: &db::Principal) -> Result<()> {
    tracing::info!(reader = me.name, "waiting for mail");
    let mut seen_version = None;
    let mut last_touch = Instant::now();
    loop {
        let version = db::data_version(conn)?;
        if seen_version != Some(version) {
            seen_version = Some(version);
            if me.kind == Kind::Agent && !db::agent_is_live(conn, me.id)? {
                bail!("{} was retired while waiting", me.name);
            }
            if db::has_unread(conn, me)? {
                return Ok(());
            }
        }
        // A waiting agent is alive; keep it clear of stale-agent retirement.
        if last_touch.elapsed() >= WAIT_TOUCH_INTERVAL {
            service::whoami(conn, caller)?;
            last_touch = Instant::now();
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

fn dispatch(conn: &mut Connection, caller: &Caller, command: Command) -> Result<String> {
    Ok(match command {
        Command::Init { .. } | Command::Hook { .. } | Command::Serve { .. } | Command::View => {
            unreachable!()
        }
        Command::Human {
            command: HumanCommand::Add { name },
        } => {
            service::add_human(conn, caller, proto::AddHumanRequest { name: name.clone() })?;
            format!("Added human {name}.\n")
        }
        Command::Group { command } => match command {
            GroupCommand::Register { group } => {
                let r = service::group_register(conn, caller, proto::GroupRequest { group })?;
                format!("Registered group @{}.\n", r.group)
            }
            GroupCommand::Join { group } => {
                let r = service::group_join(conn, caller, proto::GroupRequest { group })?;
                if r.joined {
                    format!("Joined @{}.\n", r.group)
                } else {
                    format!("Already in @{}.\n", r.group)
                }
            }
            GroupCommand::Leave { group } => {
                let r = service::group_leave(conn, caller, proto::GroupRequest { group })?;
                format!("Left @{}.\n", r.group)
            }
            GroupCommand::Remove { group, agent } => {
                let req = proto::GroupRemoveRequest { group, agent };
                let r = service::group_remove(conn, caller, req)?;
                format!("Removed {} from @{}.\n", r.agent, r.group)
            }
            GroupCommand::List => format_groups(&service::group_list(conn, caller)?.groups)?,
        },
        Command::Register { note } => {
            let req = proto::RegisterRequest {
                note,
                pwd: std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            };
            let r = service::register(conn, caller, req)?;
            format!("Welcome to the BBS. {}\n", identity_line(&r.name, &r.token))
        }
        Command::Status { text } => {
            service::set_status(conn, caller, proto::SetStatusRequest { text })?;
            "Status updated.\n".to_string()
        }
        Command::Bye => format!("Goodbye, {}.\n", service::bye(conn, caller)?.name),
        Command::Send { to, body } => {
            let req = proto::SendRequest {
                to,
                body: read_body(body)?,
            };
            let r = service::send(conn, caller, req)?;
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
            format_messages(&service::read(conn, caller, req)?.messages)?
        }
        Command::Whoami => format!("{}\n", service::whoami(conn, caller)?.name),
        Command::Agents { all } => {
            let r = service::list_agents(conn, caller, proto::ListAgentsRequest { all })?;
            format_agents(&r)
        }
    })
}

fn init(dir: &Path, db_path: &Path, human: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut conn = db::open(db_path)?;
    db::migrate(&mut conn)?;
    let name = match human {
        Some(name) => name.to_string(),
        None => std::env::var("USER").context("$USER is not set; pass --human <name>")?,
    };
    service::ensure_human(&mut conn, proto::EnsureHumanRequest { name: name.clone() })?;
    println!("BBS initialized at {}\nHuman: {name}", dir.display());
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
