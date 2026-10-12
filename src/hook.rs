//! Harness hook handlers. A hook must never break the harness session, so every
//! failure is logged and swallowed, and unbound sessions are a silent no-op.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::app::{format_messages, identity_line};
use crate::cli::Harness;
use crate::db;
use crate::proto;
use crate::service::{self, Caller};

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
    db::migrate(&mut conn)?;
    if let Some(output) = handle_event(&mut conn, harness.as_str(), &event)? {
        println!("{output}");
    }
    Ok(())
}

/// Claude Code and Codex share a hook protocol: the same event names, `Bash`
/// tool events, `additionalContext` injection, and `decision: block` on Stop.
fn handle_event(conn: &mut Connection, harness: &str, event: &Value) -> Result<Option<Value>> {
    let Some(session_id) = event["session_id"].as_str() else {
        return Ok(None);
    };
    let name = event["hook_event_name"].as_str().unwrap_or_default();

    if name == "PostToolUse" {
        bind_on_register(conn, harness, session_id, event)?;
    }
    let req = proto::SessionAgentRequest {
        harness: harness.to_string(),
        session_id: session_id.to_string(),
    };
    let bound = service::session_agent(conn, req)?;
    let Some(me) = bound.agent else {
        return Ok(None);
    };
    let caller = Caller {
        token: Some(bound.token.clone()),
        human: None,
    };

    let additional_context = |text: String| json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } });
    let mut deliver = |from_kind: Option<proto::Kind>| -> Result<Option<String>> {
        let req = proto::ReadRequest {
            from_kind: from_kind.map(Into::into),
            ..Default::default()
        };
        let msgs = service::read(conn, &caller, req)?.messages;
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
            format_messages(&msgs)?
        )))
    };

    Ok(match name {
        "PostToolUse" | "UserPromptSubmit" => deliver(None)?.map(additional_context),
        // Only humans may keep an agent from stopping; agent chatter waits for the next turn.
        "Stop" => deliver(Some(proto::Kind::Human))?
            .map(|reason| json!({ "decision": "block", "reason": reason })),
        "SessionStart" if event["source"].as_str() == Some("compact") => {
            Some(additional_context(format!(
                "You are registered on the BBS. {}",
                identity_line(&me.name, &bound.token)
            )))
        }
        _ => None,
    })
}

/// When the session runs `bbs register`, bind the session to the new agent.
fn bind_on_register(
    conn: &mut Connection,
    harness: &str,
    session_id: &str,
    event: &Value,
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
    let req = proto::BindSessionRequest {
        harness: harness.to_string(),
        session_id: session_id.to_string(),
        token,
    };
    service::bind_session(conn, req)?;
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
