//! All SQL lives here, as plain functions over a `Connection`. Keep queries to
//! portable SQL so this module is the only thing that changes for another backend.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_sessions.sql"),
    include_str!("../migrations/0003_agents_active_index.sql"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Human,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::Human => "human",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub kind: Kind,
    pub id: i64,
    pub name: String,
}

#[derive(Debug)]
pub struct AgentRow {
    pub id: i64,
    pub name: String,
    pub status: Option<String>,
    pub pwd: String,
    pub hostname: String,
    pub active_at: String,
    pub retired_at: Option<String>,
}

#[derive(Debug)]
pub struct HumanRow {
    pub name: String,
    pub active_at: String,
}

#[derive(Debug)]
pub struct MessageView {
    pub id: i64,
    pub created_at: String,
    pub from_kind: Kind,
    pub from_name: String,
    pub to_group: Option<String>,
    /// Feed only: recipient of a direct message.
    pub to_name: Option<String>,
    /// Feed only: when a direct message was read; always `None` for broadcasts.
    pub read_at: Option<String>,
    pub body: String,
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    enable_wal(&conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// WAL is persistent, so this normally just reads the mode. Switching needs an
/// exclusive lock that the busy timeout does not cover, so retry while contended.
fn enable_wal(conn: &Connection) -> Result<()> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(err) if is_busy(&err) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err).context("enabling WAL"),
        }
    }
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn schema_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(v as usize)
}

/// Bring the schema up to date. Every command calls this on open; concurrent
/// callers serialize on the write lock and re-check the version under it.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    if schema_version(conn)? == MIGRATIONS.len() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = schema_version(&tx)?;
    if current > MIGRATIONS.len() {
        bail!("BBS schema v{current} is newer than this binary supports");
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {}", i + 1))?;
        tracing::info!(version = i + 1, "applied migration");
    }
    tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
    tx.commit()?;
    Ok(())
}

// ---- humans ----------------------------------------------------------------

pub fn find_human(conn: &Connection, name: &str) -> Result<Option<Principal>> {
    Ok(conn
        .query_row(
            "SELECT id, name FROM humans WHERE lower(name) = lower(?1)",
            params![name],
            |r| {
                Ok(Principal {
                    kind: Kind::Human,
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            },
        )
        .optional()?)
}

pub fn insert_human(conn: &Connection, name: &str, now: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO humans (name, created_at, active_at) VALUES (?1, ?2, ?2)",
        params![name, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Record activity, skipping the write unless `active_at` is older than
/// `stale_before`. A no-op commit leaves `PRAGMA data_version` alone, so
/// pollers only wake for real changes.
pub fn touch_human(conn: &Connection, id: i64, now: &str, stale_before: &str) -> Result<()> {
    conn.execute(
        "UPDATE humans SET active_at = ?1 WHERE id = ?2 AND active_at < ?3",
        params![now, id, stale_before],
    )?;
    Ok(())
}

pub fn human_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM humans ORDER BY id")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

pub fn list_humans(conn: &Connection) -> Result<Vec<HumanRow>> {
    let mut stmt = conn.prepare("SELECT name, active_at FROM humans ORDER BY lower(name)")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HumanRow {
                name: r.get(0)?,
                active_at: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

// ---- agents ----------------------------------------------------------------

fn agent_principal(r: &rusqlite::Row) -> rusqlite::Result<Principal> {
    Ok(Principal {
        kind: Kind::Agent,
        id: r.get(0)?,
        name: r.get(1)?,
    })
}

pub fn agent_by_token(conn: &Connection, token: &str) -> Result<Option<Principal>> {
    Ok(conn
        .query_row(
            "SELECT id, name FROM agents WHERE token = ?1 AND retired_at IS NULL",
            params![token],
            agent_principal,
        )
        .optional()?)
}

pub fn live_agent_by_name(conn: &Connection, name: &str) -> Result<Option<Principal>> {
    Ok(conn
        .query_row(
            "SELECT id, name FROM agents WHERE lower(name) = lower(?1) AND retired_at IS NULL",
            params![name],
            agent_principal,
        )
        .optional()?)
}

pub fn agent_is_live(conn: &Connection, id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE id = ?1 AND retired_at IS NULL",
        params![id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn retired_agent_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE lower(name) = lower(?1) AND retired_at IS NOT NULL",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn token_exists(conn: &Connection, token: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE token = ?1",
        params![token],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Record activity, skipping the write unless `active_at` is older than
/// `stale_before`. A no-op commit leaves `PRAGMA data_version` alone, so
/// pollers only wake for real changes.
pub fn touch_agent(conn: &Connection, id: i64, now: &str, stale_before: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET active_at = ?1 WHERE id = ?2 AND active_at < ?3",
        params![now, id, stale_before],
    )?;
    Ok(())
}

pub fn stale_agent_ids(conn: &Connection, cutoff: &str) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT id FROM agents WHERE retired_at IS NULL AND active_at < ?1")?;
    let ids = stmt
        .query_map(params![cutoff], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// Retire an agent: free its name, archive its unread mail, drop its memberships.
pub fn retire_agent(conn: &Connection, id: i64, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET retired_at = ?1 WHERE id = ?2 AND retired_at IS NULL",
        params![now, id],
    )?;
    conn.execute(
        "UPDATE deliveries SET archived_at = ?1
         WHERE recipient_kind = 'agent' AND recipient_id = ?2 AND archived_at IS NULL",
        params![now, id],
    )?;
    conn.execute("DELETE FROM group_members WHERE agent_id = ?1", params![id])?;
    Ok(())
}

/// Lowercased names currently held by live agents or humans.
pub fn taken_names(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT lower(name) FROM agents WHERE retired_at IS NULL
         UNION SELECT lower(name) FROM humans",
    )?;
    let names = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(names)
}

/// Lowercased agent name -> most recent registration time under that name.
pub fn name_last_used(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT lower(name), MAX(created_at) FROM agents GROUP BY lower(name)")?;
    let map = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(map)
}

pub struct NewAgent<'a> {
    pub name: &'a str,
    pub token: &'a str,
    pub status: Option<&'a str>,
    pub pwd: &'a str,
    pub hostname: &'a str,
    pub now: &'a str,
}

pub fn insert_agent(conn: &Connection, a: &NewAgent) -> Result<i64> {
    let status_at = a.status.map(|_| a.now);
    conn.execute(
        "INSERT INTO agents (name, token, status, status_at, pwd, hostname, created_at, active_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![
            a.name, a.token, a.status, status_at, a.pwd, a.hostname, a.now
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn set_status(conn: &Connection, id: i64, status: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE agents SET status = ?1, status_at = ?2 WHERE id = ?3",
        params![status, now, id],
    )?;
    Ok(())
}

pub fn list_agents(conn: &Connection, include_retired: bool) -> Result<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, status, pwd, hostname, active_at, retired_at FROM agents
         WHERE retired_at IS NULL OR ?1
         ORDER BY retired_at IS NOT NULL, lower(name), id",
    )?;
    let rows = stmt
        .query_map(params![include_retired], |r| {
            Ok(AgentRow {
                id: r.get(0)?,
                name: r.get(1)?,
                status: r.get(2)?,
                pwd: r.get(3)?,
                hostname: r.get(4)?,
                active_at: r.get(5)?,
                retired_at: r.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

// ---- groups ----------------------------------------------------------------

pub fn group_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM bbs_groups WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn insert_group(conn: &Connection, name: &str, now: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO bbs_groups (name, created_at) VALUES (?1, ?2)",
        params![name, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Returns false if the agent was already a member.
pub fn join_group(conn: &Connection, group_id: i64, agent_id: i64, now: &str) -> Result<bool> {
    let n = conn.execute(
        "INSERT INTO group_members (group_id, agent_id, joined_at) VALUES (?1, ?2, ?3)
         ON CONFLICT DO NOTHING",
        params![group_id, agent_id, now],
    )?;
    Ok(n > 0)
}

/// Returns false if the agent was not a member.
pub fn leave_group(conn: &Connection, group_id: i64, agent_id: i64) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM group_members WHERE group_id = ?1 AND agent_id = ?2",
        params![group_id, agent_id],
    )?;
    Ok(n > 0)
}

pub fn is_member(conn: &Connection, group_id: i64, agent_id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM group_members WHERE group_id = ?1 AND agent_id = ?2",
        params![group_id, agent_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn share_group(conn: &Connection, a: i64, b: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM group_members x
         JOIN group_members y ON x.group_id = y.group_id
         WHERE x.agent_id = ?1 AND y.agent_id = ?2",
        params![a, b],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn group_member_ids(conn: &Connection, group_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT m.agent_id FROM group_members m JOIN agents a ON a.id = m.agent_id
         WHERE m.group_id = ?1 AND a.retired_at IS NULL ORDER BY m.agent_id",
    )?;
    let ids = stmt
        .query_map(params![group_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// (group id, group name) for every group `agent_id` belongs to.
pub fn member_groups(conn: &Connection, agent_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, g.name FROM group_members m JOIN bbs_groups g ON g.id = m.group_id
         WHERE m.agent_id = ?1 ORDER BY g.name",
    )?;
    let groups = stmt
        .query_map(params![agent_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(groups)
}

/// (group name, member names) for every group, or only those `agent_id` belongs to.
pub fn list_groups(conn: &Connection, agent_id: Option<i64>) -> Result<Vec<(String, Vec<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT g.name, a.name FROM bbs_groups g
         LEFT JOIN group_members m ON m.group_id = g.id
         LEFT JOIN agents a ON a.id = m.agent_id
         WHERE ?1 IS NULL OR g.id IN (SELECT group_id FROM group_members WHERE agent_id = ?1)
         ORDER BY g.name, lower(a.name)",
    )?;
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let rows = stmt.query_map(params![agent_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (group, member) = row?;
        if groups.last().is_none_or(|(g, _)| *g != group) {
            groups.push((group, Vec::new()));
        }
        if let Some(member) = member {
            groups.last_mut().unwrap().1.push(member);
        }
    }
    Ok(groups)
}

/// Agent id -> names of the groups it belongs to.
pub fn agent_groups(conn: &Connection) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT m.agent_id, g.name FROM group_members m
         JOIN bbs_groups g ON g.id = m.group_id ORDER BY g.name",
    )?;
    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (id, group) = row?;
        map.entry(id).or_default().push(group);
    }
    Ok(map)
}

// ---- messages --------------------------------------------------------------

pub fn insert_message(
    conn: &Connection,
    from: &Principal,
    to_group_id: Option<i64>,
    body: &str,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages (from_kind, from_id, to_group_id, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from.kind.as_str(), from.id, to_group_id, body, now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn insert_delivery(conn: &Connection, message_id: i64, kind: Kind, id: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO deliveries (message_id, recipient_kind, recipient_id) VALUES (?1, ?2, ?3)",
        params![message_id, kind.as_str(), id],
    )?;
    Ok(())
}

const INBOX_SELECT: &str = "
    SELECT m.id, m.created_at, m.from_kind,
           CASE m.from_kind WHEN 'agent' THEN a.name ELSE h.name END,
           g.name, m.body
    FROM deliveries d
    JOIN messages m ON m.id = d.message_id
    LEFT JOIN agents a ON m.from_kind = 'agent' AND a.id = m.from_id
    LEFT JOIN humans h ON m.from_kind = 'human' AND h.id = m.from_id
    LEFT JOIN bbs_groups g ON g.id = m.to_group_id
    WHERE d.recipient_kind = ?1 AND d.recipient_id = ?2";

fn message_view(r: &rusqlite::Row) -> rusqlite::Result<MessageView> {
    let from_kind: String = r.get(2)?;
    Ok(MessageView {
        id: r.get(0)?,
        created_at: r.get(1)?,
        from_kind: if from_kind == "human" {
            Kind::Human
        } else {
            Kind::Agent
        },
        from_name: r.get(3)?,
        to_group: r.get(4)?,
        to_name: None,
        read_at: None,
        body: r.get(5)?,
    })
}

pub fn has_unread(conn: &Connection, me: &Principal) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM deliveries
         WHERE recipient_kind = ?1 AND recipient_id = ?2 AND archived_at IS NULL)",
        params![me.kind.as_str(), me.id],
        |r| r.get(0),
    )?;
    Ok(n != 0)
}

/// Fetch unread messages oldest-first and mark them archived. `from_kind`
/// restricts this to messages sent by humans or by agents.
pub fn take_unread(
    conn: &Connection,
    me: &Principal,
    from_kind: Option<Kind>,
    limit: Option<u32>,
    now: &str,
) -> Result<Vec<MessageView>> {
    let sql = format!(
        "{INBOX_SELECT} AND d.archived_at IS NULL AND (?3 IS NULL OR m.from_kind = ?3)
         ORDER BY m.created_at, m.id LIMIT ?4"
    );
    let limit = limit.map_or(i64::MAX, i64::from);
    let from_kind = from_kind.map(Kind::as_str);
    let msgs: Vec<MessageView> = conn
        .prepare(&sql)?
        .query_map(
            params![me.kind.as_str(), me.id, from_kind, limit],
            message_view,
        )?
        .collect::<Result<_, _>>()?;
    let mut archive = conn.prepare(
        "UPDATE deliveries SET archived_at = ?1
         WHERE message_id = ?2 AND recipient_kind = ?3 AND recipient_id = ?4",
    )?;
    for m in &msgs {
        archive.execute(params![now, m.id, me.kind.as_str(), me.id])?;
    }
    Ok(msgs)
}

pub fn archived_since(
    conn: &Connection,
    me: &Principal,
    since: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<MessageView>> {
    let sql = format!(
        "{INBOX_SELECT} AND d.archived_at IS NOT NULL AND (?3 IS NULL OR m.created_at >= ?3)
         ORDER BY m.created_at, m.id LIMIT ?4"
    );
    let limit = limit.map_or(i64::MAX, i64::from);
    let msgs = conn
        .prepare(&sql)?
        .query_map(params![me.kind.as_str(), me.id, since, limit], message_view)?
        .collect::<Result<_, _>>()?;
    Ok(msgs)
}

// ---- harness sessions ------------------------------------------------------

/// Bind a harness session to an agent, returning the agent it was previously bound to.
pub fn bind_session(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    agent_id: i64,
    now: &str,
) -> Result<Option<i64>> {
    let previous: Option<i64> = conn
        .query_row(
            "SELECT agent_id FROM sessions WHERE harness = ?1 AND session_id = ?2",
            params![harness, session_id],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT INTO sessions (harness, session_id, agent_id, bound_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (harness, session_id) DO UPDATE SET agent_id = ?3, bound_at = ?4",
        params![harness, session_id, agent_id, now],
    )?;
    Ok(previous.filter(|&id| id != agent_id))
}

/// The live agent bound to a harness session, with its token.
pub fn session_agent(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> Result<Option<(Principal, String)>> {
    Ok(conn
        .query_row(
            "SELECT a.id, a.name, a.token FROM sessions s JOIN agents a ON a.id = s.agent_id
             WHERE s.harness = ?1 AND s.session_id = ?2 AND a.retired_at IS NULL",
            params![harness, session_id],
            |r| Ok((agent_principal(r)?, r.get(2)?)),
        )
        .optional()?)
}

// ---- feed (bbs view) -------------------------------------------------------

/// The most recent `limit` messages across the whole board, oldest first. Each
/// broadcast appears once, regardless of how many deliveries it fanned out to.
pub fn recent_messages(conn: &Connection, limit: u32) -> Result<Vec<MessageView>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.created_at, m.from_kind, COALESCE(fa.name, fh.name), g.name,
                COALESCE(ta.name, th.name), d.archived_at, m.body
         FROM messages m
         LEFT JOIN agents fa ON m.from_kind = 'agent' AND fa.id = m.from_id
         LEFT JOIN humans fh ON m.from_kind = 'human' AND fh.id = m.from_id
         LEFT JOIN bbs_groups g ON g.id = m.to_group_id
         LEFT JOIN deliveries d ON m.to_group_id IS NULL AND d.message_id = m.id
         LEFT JOIN agents ta ON d.recipient_kind = 'agent' AND ta.id = d.recipient_id
         LEFT JOIN humans th ON d.recipient_kind = 'human' AND th.id = d.recipient_id
         ORDER BY m.id DESC
         LIMIT ?1",
    )?;
    let mut rows: Vec<MessageView> = stmt
        .query_map(params![limit], |r| {
            let from_kind: String = r.get(2)?;
            Ok(MessageView {
                id: r.get(0)?,
                created_at: r.get(1)?,
                from_kind: if from_kind == "human" {
                    Kind::Human
                } else {
                    Kind::Agent
                },
                from_name: r.get(3)?,
                to_group: r.get(4)?,
                to_name: r.get(5)?,
                read_at: r.get(6)?,
                body: r.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    rows.reverse();
    Ok(rows)
}

pub fn data_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "data_version", |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_shows_broadcasts_once_and_dm_read_times() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let t = "2026-10-09T00:00:00.000000Z";
        let will = Principal {
            kind: Kind::Human,
            id: insert_human(&conn, "will", t).unwrap(),
            name: "will".into(),
        };
        let agent = |name: &str, token: &str| {
            let a = NewAgent {
                name,
                token,
                status: None,
                pwd: "/",
                hostname: "h",
                now: t,
            };
            Principal {
                kind: Kind::Agent,
                id: insert_agent(&conn, &a).unwrap(),
                name: name.into(),
            }
        };
        let (a, b, c) = (
            agent("Aiko", "t1"),
            agent("Bram", "t2"),
            agent("Cian", "t3"),
        );
        let g = insert_group(&conn, "acme", t).unwrap();

        let m = insert_message(&conn, &a, Some(g), "hello all", t).unwrap();
        for p in [&b, &c, &will] {
            insert_delivery(&conn, m, p.kind, p.id).unwrap();
        }
        let m = insert_message(&conn, &will, None, "hi Bram", t).unwrap();
        insert_delivery(&conn, m, Kind::Agent, b.id).unwrap();

        let feed = recent_messages(&conn, 10).unwrap();
        assert_eq!(feed.len(), 2);
        assert_eq!(
            (feed[0].to_group.as_deref(), feed[0].to_name.as_deref()),
            (Some("acme"), None)
        );
        assert_eq!(feed[0].read_at, None);
        assert_eq!(feed[1].from_name, "will");
        assert_eq!(feed[1].to_name.as_deref(), Some("Bram"));
        assert_eq!(feed[1].read_at, None);

        take_unread(&conn, &b, None, None, "2026-10-09T01:00:00.000000Z").unwrap();
        let feed = recent_messages(&conn, 10).unwrap();
        assert_eq!(feed[0].read_at, None, "broadcast read times stay hidden");
        assert_eq!(
            feed[1].read_at.as_deref(),
            Some("2026-10-09T01:00:00.000000Z")
        );
    }
}
