use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use rand::seq::IndexedRandom;
use rusqlite::{Connection, TransactionBehavior};

use crate::cli::{Cli, Command, GroupCommand, HumanCommand};
use crate::db::{self, Kind, Principal};
use crate::names::NAMES;

const RETIRE_AFTER_DAYS: i64 = 7;
const MAX_BODY_BYTES: usize = 64 * 1024;
const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const WAIT_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// `active_at` is refreshed at most this often.
const ACTIVE_AT_DEBOUNCE: chrono::Duration = chrono::Duration::seconds(60);

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

pub(crate) fn state_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/state/bbs"))
}

pub(crate) fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Cutoff for `db::touch_*`: only `active_at` values older than this are rewritten.
pub(crate) fn touch_cutoff(now: DateTime<Utc>) -> String {
    fmt_time(now - ACTIVE_AT_DEBOUNCE)
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
        _ => {}
    }
    if !db_path.exists() {
        bail!("BBS is not initialized; run `bbs init`");
    }
    let mut conn = db::open(&db_path)?;
    db::migrate(&mut conn)?;
    if let Command::View = cli.command {
        with_ctx(&mut conn, &cli, |ctx| ctx.human())?;
        return crate::view::run(&conn);
    }
    let wait = matches!(cli.command, Command::Read { wait: true, .. });
    loop {
        let out = with_ctx(&mut conn, &cli, |ctx| dispatch(ctx, &cli.command))?;
        if !out.is_empty() || !wait {
            print!("{out}");
            return Ok(());
        }
        // Another reader of the same inbox may win the race; keep waiting until we get mail.
        wait_for_unread(&mut conn, &cli)?;
    }
}

/// Run `f` in its own immediate (write-locking) transaction.
fn with_ctx<T>(conn: &mut Connection, cli: &Cli, f: impl FnOnce(&Ctx) -> Result<T>) -> Result<T> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = Utc::now();
    let ctx = Ctx {
        conn: &tx,
        now,
        now_s: fmt_time(now),
        token: cli.token.as_deref(),
        as_human: cli.as_human.as_deref(),
    };
    let out = f(&ctx)?;
    tx.commit()?;
    Ok(out)
}

fn dispatch(ctx: &Ctx, command: &Command) -> Result<String> {
    match command {
        Command::Init { .. } | Command::Hook { .. } | Command::View => unreachable!(),
        Command::Human {
            command: HumanCommand::Add { name },
        } => human_add(ctx, name),
        Command::Group { command } => match command {
            GroupCommand::Register { group } => group_register(ctx, group),
            GroupCommand::Join { group } => group_join(ctx, group),
            GroupCommand::Leave { group } => group_leave(ctx, group),
            GroupCommand::Remove { group, agent } => group_remove(ctx, group, agent),
            GroupCommand::List => group_list(ctx),
        },
        Command::Register { note } => register(ctx, note.as_deref()),
        Command::Status { text } => status(ctx, text),
        Command::Bye => bye(ctx),
        Command::Send { to, body } => send(ctx, to, body.clone()),
        Command::Read {
            limit,
            archived,
            since,
            ..
        } => read(ctx, *limit, *archived, since.as_deref()),
        Command::Whoami => Ok(format!("{}\n", ctx.actor()?.name)),
        Command::Agents { all } => agents(ctx, *all),
    }
}

/// Block until the caller has unread mail. Never holds a transaction while idle:
/// `PRAGMA data_version` changes only when another connection commits, so we
/// poll it once a second and run the unread query only after a change.
fn wait_for_unread(conn: &mut Connection, cli: &Cli) -> Result<()> {
    let me = with_ctx(conn, cli, |ctx| ctx.actor())?;
    tracing::info!(reader = me.name, "waiting for mail");
    let mut seen_version = None;
    let mut last_touch = Instant::now();
    loop {
        let version: i64 = conn.pragma_query_value(None, "data_version", |r| r.get(0))?;
        if seen_version != Some(version) {
            seen_version = Some(version);
            if me.kind == Kind::Agent && !db::agent_is_live(conn, me.id)? {
                bail!("{} was retired while waiting", me.name);
            }
            if db::has_unread(conn, &me)? {
                return Ok(());
            }
        }
        // A waiting agent is alive; keep it clear of stale-agent retirement.
        if last_touch.elapsed() >= WAIT_TOUCH_INTERVAL {
            with_ctx(conn, cli, |ctx| ctx.actor())?;
            last_touch = Instant::now();
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

struct Ctx<'a> {
    conn: &'a Connection,
    now: DateTime<Utc>,
    now_s: String,
    token: Option<&'a str>,
    as_human: Option<&'a str>,
}

impl Ctx<'_> {
    /// Resolve the caller and record activity.
    fn actor(&self) -> Result<Principal> {
        if let Some(token) = self.token {
            let token = token.trim().to_ascii_lowercase();
            let Some(agent) = db::agent_by_token(self.conn, &token)? else {
                bail!("unknown or retired token; run `bbs register`");
            };
            db::touch_agent(self.conn, agent.id, &self.now_s, &touch_cutoff(self.now))?;
            return Ok(agent);
        }
        let name = default_human(self.as_human)?;
        let Some(human) = db::find_human(self.conn, &name)? else {
            bail!("unknown human {name:?}; run `bbs init` or `bbs human add {name}`");
        };
        db::touch_human(self.conn, human.id, &self.now_s, &touch_cutoff(self.now))?;
        Ok(human)
    }

    fn agent(&self) -> Result<Principal> {
        if self.token.is_none() {
            bail!("this command requires --token");
        }
        self.actor()
    }

    fn human(&self) -> Result<Principal> {
        if self.token.is_some() {
            bail!("this command is for humans only");
        }
        self.actor()
    }
}

fn default_human(as_human: Option<&str>) -> Result<String> {
    match as_human {
        Some(name) => Ok(name.to_string()),
        None => std::env::var("USER").context("$USER is not set; pass --as <human>"),
    }
}

fn validate_human_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok {
        bail!("invalid human name {name:?}: use letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

fn validate_group_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c));
    if !ok {
        bail!("invalid group name {name:?}: use lowercase letters, digits, '_' or '-'");
    }
    Ok(())
}

fn add_human(conn: &Connection, name: &str, now: &str) -> Result<()> {
    validate_human_name(name)?;
    if db::find_human(conn, name)?.is_some() {
        bail!("human {name:?} already exists");
    }
    if db::live_agent_by_name(conn, name)?.is_some() {
        bail!("{name:?} is in use by an agent");
    }
    db::insert_human(conn, name, now)?;
    tracing::info!(human = name, "added human");
    Ok(())
}

fn init(dir: &Path, db_path: &Path, human: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut conn = db::open(db_path)?;
    db::migrate(&mut conn)?;
    let name = default_human(human)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = fmt_time(Utc::now());
    let existing = db::find_human(&tx, &name)?;
    if existing.is_none() {
        add_human(&tx, &name, &now)?;
    }
    tx.commit()?;
    println!("BBS initialized at {}\nHuman: {name}", dir.display());
    Ok(())
}

fn human_add(ctx: &Ctx, name: &str) -> Result<String> {
    ctx.human()?;
    add_human(ctx.conn, name, &ctx.now_s)?;
    Ok(format!("Added human {name}.\n"))
}

fn group_register(ctx: &Ctx, group: &str) -> Result<String> {
    let me = ctx.human()?;
    validate_group_name(group)?;
    if db::group_id(ctx.conn, group)?.is_some() {
        bail!("group @{group} already exists");
    }
    db::insert_group(ctx.conn, group, &ctx.now_s)?;
    tracing::info!(group, by = me.name, "registered group");
    Ok(format!("Registered group @{group}.\n"))
}

fn require_group(conn: &Connection, group: &str) -> Result<i64> {
    let group = group.strip_prefix('@').unwrap_or(group);
    db::group_id(conn, group)?.with_context(|| format!("no such group @{group}"))
}

fn group_join(ctx: &Ctx, group: &str) -> Result<String> {
    let me = ctx.agent()?;
    let gid = require_group(ctx.conn, group)?;
    let group = group.trim_start_matches('@');
    if db::join_group(ctx.conn, gid, me.id, &ctx.now_s)? {
        tracing::info!(agent = me.name, group, "joined group");
        Ok(format!("Joined @{group}.\n"))
    } else {
        Ok(format!("Already in @{group}.\n"))
    }
}

fn group_leave(ctx: &Ctx, group: &str) -> Result<String> {
    let me = ctx.agent()?;
    let gid = require_group(ctx.conn, group)?;
    let group = group.trim_start_matches('@');
    if !db::is_member(ctx.conn, gid, me.id)? {
        bail!("you are not in @{group}");
    }
    broadcast(ctx, &me, gid, &format!("{} left @{group}.", me.name))?;
    db::leave_group(ctx.conn, gid, me.id)?;
    tracing::info!(agent = me.name, group, "left group");
    Ok(format!("Left @{group}.\n"))
}

fn group_remove(ctx: &Ctx, group: &str, agent: &str) -> Result<String> {
    let me = ctx.human()?;
    let gid = require_group(ctx.conn, group)?;
    let group = group.trim_start_matches('@');
    let Some(target) = db::live_agent_by_name(ctx.conn, agent)? else {
        bail!("no live agent named {agent:?}");
    };
    if !db::is_member(ctx.conn, gid, target.id)? {
        bail!("{} is not in @{group}", target.name);
    }
    // Notify before removing so the removed agent learns why the group went quiet.
    let notice = format!("{} removed {} from @{group}.", me.name, target.name);
    broadcast(ctx, &me, gid, &notice)?;
    db::leave_group(ctx.conn, gid, target.id)?;
    tracing::info!(
        agent = target.name,
        group,
        by = me.name,
        "removed from group"
    );
    Ok(format!("Removed {} from @{group}.\n", target.name))
}

fn group_list(ctx: &Ctx) -> Result<String> {
    let me = ctx.actor()?;
    let only = (me.kind == Kind::Agent).then_some(me.id);
    let mut out = String::new();
    for (group, members) in db::list_groups(ctx.conn, only)? {
        let members = if members.is_empty() {
            "(no agents)".to_string()
        } else {
            members.join(", ")
        };
        writeln!(out, "@{group}: {members}")?;
    }
    Ok(out)
}

fn pick_name(conn: &Connection) -> Result<String> {
    let taken = db::taken_names(conn)?;
    let last_used = db::name_last_used(conn)?;
    let free: Vec<&str> = NAMES
        .iter()
        .copied()
        .filter(|n| !taken.contains(&n.to_lowercase()))
        .collect();
    let never_used: Vec<&str> = free
        .iter()
        .copied()
        .filter(|n| !last_used.contains_key(&n.to_lowercase()))
        .collect();
    if let Some(name) = never_used.choose(&mut rand::rng()) {
        return Ok(name.to_string());
    }
    // Every free name has been used before: recycle the least recently used one.
    free.iter()
        .min_by_key(|n| last_used.get(&n.to_lowercase()))
        .map(|n| n.to_string())
        .with_context(|| format!("name pool exhausted: all {} names are in use", NAMES.len()))
}

fn new_token(conn: &Connection) -> Result<String> {
    loop {
        let bytes: [u8; 4] = rand::random();
        let token = data_encoding::BASE32_NOPAD
            .encode(&bytes)
            .to_ascii_lowercase();
        if !db::token_exists(conn, &token)? {
            return Ok(token);
        }
    }
}

fn register(ctx: &Ctx, note: Option<&str>) -> Result<String> {
    if ctx.token.is_some() {
        bail!("already registered; `bbs bye` first to register again");
    }
    let cutoff = fmt_time(ctx.now - chrono::Duration::days(RETIRE_AFTER_DAYS));
    for id in db::stale_agent_ids(ctx.conn, &cutoff)? {
        db::retire_agent(ctx.conn, id, &ctx.now_s)?;
        tracing::info!(agent_id = id, "retired stale agent");
    }
    let name = pick_name(ctx.conn)?;
    let token = new_token(ctx.conn)?;
    let pwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    let id = db::insert_agent(
        ctx.conn,
        &db::NewAgent {
            name: &name,
            token: &token,
            status: note,
            pwd: &pwd,
            hostname: &hostname,
            now: &ctx.now_s,
        },
    )?;
    tracing::info!(agent_id = id, name, pwd, hostname, "registered agent");
    Ok(format!(
        "Welcome to the BBS. {}\n",
        identity_line(&name, &token)
    ))
}

pub(crate) fn identity_line(name: &str, token: &str) -> String {
    format!("Your name is \"{name}\", access BBS with `bbs --token {token} ...`")
}

fn status(ctx: &Ctx, text: &str) -> Result<String> {
    let me = ctx.agent()?;
    db::set_status(ctx.conn, me.id, text, &ctx.now_s)?;
    Ok("Status updated.\n".to_string())
}

fn bye(ctx: &Ctx) -> Result<String> {
    let me = ctx.agent()?;
    db::retire_agent(ctx.conn, me.id, &ctx.now_s)?;
    tracing::info!(agent = me.name, "agent retired");
    Ok(format!("Goodbye, {}.\n", me.name))
}

fn read_body(body: Option<String>) -> Result<String> {
    let body = match body {
        Some(b) if b != "-" => b,
        _ => {
            let mut s = String::new();
            std::io::stdin()
                .take(MAX_BODY_BYTES as u64 + 1)
                .read_to_string(&mut s)
                .context("reading message body from stdin")?;
            s
        }
    };
    if body.trim().is_empty() {
        bail!("message body is empty");
    }
    if body.len() > MAX_BODY_BYTES {
        bail!("message body exceeds {} KiB", MAX_BODY_BYTES / 1024);
    }
    Ok(body)
}

/// Deliver `body` from `me` to every member of the group and every human,
/// except `me`. Returns (recipients, recipients visible to `me`): humans
/// silently receive every broadcast, so agents only see the agent count.
fn broadcast(ctx: &Ctx, me: &Principal, gid: i64, body: &str) -> Result<(usize, usize)> {
    let msg = db::insert_message(ctx.conn, me, Some(gid), body, &ctx.now_s)?;
    let mut count = 0;
    let mut visible = 0;
    let agents = db::group_member_ids(ctx.conn, gid)?
        .into_iter()
        .map(|id| (Kind::Agent, id));
    let humans = db::human_ids(ctx.conn)?
        .into_iter()
        .map(|id| (Kind::Human, id));
    for (kind, id) in agents.chain(humans) {
        if kind == me.kind && id == me.id {
            continue;
        }
        db::insert_delivery(ctx.conn, msg, kind, id)?;
        count += 1;
        if me.kind == Kind::Human || kind == Kind::Agent {
            visible += 1;
        }
    }
    Ok((count, visible))
}

fn send(ctx: &Ctx, to: &str, body: Option<String>) -> Result<String> {
    let me = ctx.actor()?;
    let body = read_body(body)?;

    if let Some(group) = to.strip_prefix('@') {
        let gid = require_group(ctx.conn, group)?;
        if me.kind == Kind::Agent && !db::is_member(ctx.conn, gid, me.id)? {
            bail!("you are not a member of @{group}");
        }
        let (count, visible) = broadcast(ctx, &me, gid, &body)?;
        tracing::info!(from = me.name, to = %format!("@{group}"), recipients = count, bytes = body.len(), "sent broadcast");
        return Ok(format!("Sent to @{group} ({visible} recipients).\n"));
    }

    let target = match db::find_human(ctx.conn, to)? {
        Some(h) => h,
        None => match db::live_agent_by_name(ctx.conn, to)? {
            Some(a) => a,
            None if db::retired_agent_exists(ctx.conn, to)? => bail!("{to} is offline (retired)"),
            None => bail!("no such agent or human: {to}"),
        },
    };
    if target.kind == me.kind && target.id == me.id {
        bail!("cannot send a message to yourself");
    }
    if me.kind == Kind::Agent && target.kind == Kind::Human {
        bail!(
            "agents cannot message humans; talk to {} through your harness",
            target.name
        );
    }
    if me.kind == Kind::Agent
        && target.kind == Kind::Agent
        && !db::share_group(ctx.conn, me.id, target.id)?
    {
        bail!("{} is not in any of your groups", target.name);
    }
    let msg = db::insert_message(ctx.conn, &me, None, &body, &ctx.now_s)?;
    db::insert_delivery(ctx.conn, msg, target.kind, target.id)?;
    tracing::info!(
        from = me.name,
        to = target.name,
        bytes = body.len(),
        "sent message"
    );
    Ok(format!("Sent to {}.\n", target.name))
}

fn parse_since(s: &str, now: DateTime<Utc>) -> Result<String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(fmt_time(t.with_timezone(&Utc)));
    }
    let d = humantime::parse_duration(s).with_context(|| {
        format!("invalid --since {s:?}: expected RFC3339 or a duration like 2h")
    })?;
    Ok(fmt_time(now - chrono::Duration::from_std(d)?))
}

fn read(ctx: &Ctx, limit: Option<u32>, archived: bool, since: Option<&str>) -> Result<String> {
    let me = ctx.actor()?;
    let msgs = if archived {
        let since = since.map(|s| parse_since(s, ctx.now)).transpose()?;
        db::archived_since(ctx.conn, &me, since.as_deref(), limit)?
    } else {
        let msgs = db::take_unread(ctx.conn, &me, None, limit, &ctx.now_s)?;
        if !msgs.is_empty() {
            tracing::info!(reader = me.name, count = msgs.len(), "archived messages");
        }
        msgs
    };
    format_messages(&me, &msgs)
}

pub(crate) fn format_messages(me: &Principal, msgs: &[db::MessageView]) -> Result<String> {
    let mut out = String::new();
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let human = if m.from_kind == Kind::Human {
            " (human)"
        } else {
            ""
        };
        let to = match &m.to_group {
            Some(g) => format!("@{g}"),
            None => me.name.clone(),
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

fn agents(ctx: &Ctx, all: bool) -> Result<String> {
    let me = ctx.actor()?;
    let groups = db::agent_groups(ctx.conn)?;
    let mut rows = db::list_agents(ctx.conn, all && me.kind == Kind::Human)?;
    if me.kind == Kind::Agent {
        let mine: HashSet<&String> = groups.get(&me.id).into_iter().flatten().collect();
        rows.retain(|a| {
            a.id == me.id
                || groups
                    .get(&a.id)
                    .into_iter()
                    .flatten()
                    .any(|g| mine.contains(g))
        });
    }

    let mut table: Vec<[String; 6]> = vec![[
        "NAME".into(),
        "KIND".into(),
        "GROUPS".into(),
        "ACTIVE_AT".into(),
        "WHERE".into(),
        "STATUS".into(),
    ]];
    // Agents cannot message humans, so humans are not listed for them.
    let humans = match me.kind {
        Kind::Human => db::list_humans(ctx.conn)?,
        Kind::Agent => Vec::new(),
    };
    for h in humans {
        table.push([
            h.name,
            "human".into(),
            "*".into(),
            display_time(&h.active_at),
            String::new(),
            String::new(),
        ]);
    }
    let empty = Vec::new();
    for a in rows {
        let kind = if a.retired_at.is_some() {
            "retired"
        } else {
            "agent"
        };
        let g = groups.get(&a.id).unwrap_or(&empty).join(",");
        table.push([
            a.name,
            kind.into(),
            if g.is_empty() { "-".into() } else { g },
            display_time(&a.active_at),
            format!("{}:{}", a.hostname, a.pwd),
            a.status.unwrap_or_default(),
        ]);
    }
    Ok(render_table(&table))
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
