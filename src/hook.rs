//! Harness hook handlers. A hook must never break the harness session, so every
//! failure is logged and swallowed, and unbound sessions are a silent no-op.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};

use crate::app::{fmt_time, format_messages, identity_line};
use crate::cli::Harness;
use crate::db::{self, Kind};

pub fn run(harness: Harness, db_path: &Path) -> Result<()> {
    if let Err(err) = handle(harness, db_path) {
        tracing::error!(harness = harness.as_str(), "hook failed: {err:#}");
    }
    Ok(())
}

fn handle(harness: Harness, db_path: &Path) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if !db_path.exists() {
        return Ok(());
    }
    let event: Value = serde_json::from_str(&input).context("parsing hook input")?;
    let mut conn = db::open(db_path)?;
    db::check_schema(&conn)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let output = handle_event(&tx, harness.as_str(), &event)?;
    tx.commit()?;
    if let Some(output) = output {
        println!("{output}");
    }
    Ok(())
}

/// Claude Code and Codex share a hook protocol: the same event names, `Bash`
/// tool events, `additionalContext` injection, and `decision: block` on Stop.
fn handle_event(conn: &Connection, harness: &str, event: &Value) -> Result<Option<Value>> {
    let Some(session_id) = event["session_id"].as_str() else {
        return Ok(None);
    };
    let name = event["hook_event_name"].as_str().unwrap_or_default();
    let now = fmt_time(Utc::now());

    if name == "PostToolUse" {
        bind_on_register(conn, harness, session_id, event, &now)?;
    }
    let Some((me, token)) = db::session_agent(conn, harness, session_id)? else {
        return Ok(None);
    };

    let additional_context = |text: String| json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } });
    let deliver = |from_kind: Option<Kind>| -> Result<Option<String>> {
        db::touch_agent(conn, me.id, &now)?;
        let msgs = db::take_unread(conn, &me, from_kind, None, &now)?;
        if msgs.is_empty() {
            return Ok(None);
        }
        tracing::info!(
            reader = me.name,
            count = msgs.len(),
            event = name,
            "delivered via hook"
        );
        Ok(Some(format!(
            "New BBS messages for {}:\n\n{}",
            me.name,
            format_messages(&me, &msgs)?
        )))
    };

    Ok(match name {
        "PostToolUse" | "UserPromptSubmit" => deliver(None)?.map(additional_context),
        // Only humans may keep an agent from stopping; agent chatter waits for the next turn.
        "Stop" => deliver(Some(Kind::Human))?
            .map(|reason| json!({ "decision": "block", "reason": reason })),
        "SessionStart" if event["source"].as_str() == Some("compact") => {
            Some(additional_context(format!(
                "You are registered on the BBS. {}",
                identity_line(&me.name, &token)
            )))
        }
        _ => None,
    })
}

/// When the session runs `bbs register`, bind the session to the new agent.
fn bind_on_register(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    event: &Value,
    now: &str,
) -> Result<()> {
    if event["tool_name"].as_str() != Some("Bash") {
        return Ok(());
    }
    let command = event["tool_input"]["command"].as_str().unwrap_or_default();
    if !(command.contains("bbs") && command.contains("register")) {
        return Ok(());
    }
    // Claude Code reports `{stdout, stderr, ...}`; Codex reports the output as a string.
    let response = &event["tool_response"];
    let stdout = response["stdout"]
        .as_str()
        .or(response.as_str())
        .unwrap_or_default();
    let Some(token) = parse_welcome_token(stdout) else {
        return Ok(());
    };
    let Some(agent) = db::agent_by_token(conn, &token)? else {
        return Ok(());
    };
    if let Some(previous) = db::bind_session(conn, harness, session_id, agent.id, now)? {
        // A session holds one identity; re-registering retires the old one.
        db::retire_agent(conn, previous, now)?;
        tracing::info!(agent_id = previous, "retired agent replaced in session");
    }
    tracing::info!(harness, session_id, agent = agent.name, "bound session");
    Ok(())
}

fn parse_welcome_token(stdout: &str) -> Option<String> {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("Welcome to the BBS."))?;
    let rest = line.split("--token ").nth(1)?;
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!token.is_empty()).then_some(token)
}
