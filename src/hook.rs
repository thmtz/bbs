//! Harness hook handlers. A hook must never break the harness session, so every
//! failure is logged and swallowed, and unbound sessions are a silent no-op.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::app::{format_messages, identity_line};
use crate::backend::Backend;
use crate::cli::Harness;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

pub async fn run(harness: Harness, dir: &Path) -> Result<()> {
    if let Err(err) = handle(harness, dir).await {
        tracing::error!(harness = harness.as_str(), "hook failed: {err:#}");
    }
    Ok(())
}

async fn handle(harness: Harness, dir: &Path) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let config = Config::load()?;
    if config.server.is_none() && !dir.join("bbs.db").exists() {
        return Ok(());
    }
    let event: Value = serde_json::from_str(&input).context("parsing hook input")?;
    // A down server fails within the connect timeout, so it never stalls the harness.
    let mut backend = Backend::open(&config, dir, Caller::default()).await?;
    if let Some(output) = handle_event(&mut backend, harness.as_str(), &event).await? {
        println!("{output}");
    }
    Ok(())
}

/// Claude Code and Codex share a hook protocol: the same event names, `Bash`
/// tool events, `additionalContext` injection, and `decision: block` on Stop.
async fn handle_event(
    backend: &mut Backend,
    harness: &str,
    event: &Value,
) -> Result<Option<Value>> {
    let Some(session_id) = event["session_id"].as_str() else {
        return Ok(None);
    };
    let name = event["hook_event_name"].as_str().unwrap_or_default();

    if name == "PostToolUse" {
        bind_on_register(backend, harness, session_id, event).await?;
    }
    let req = proto::SessionAgentRequest {
        harness: harness.to_string(),
        session_id: session_id.to_string(),
    };
    let bound = backend.session_agent(req).await?;
    let Some(me) = bound.agent else {
        return Ok(None);
    };
    backend.set_caller(Caller {
        token: Some(bound.token.clone()),
        human: None,
        ..Default::default()
    });

    let additional_context = |text: String| json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } });
    Ok(match name {
        "PostToolUse" | "UserPromptSubmit" => deliver(backend, &me.name, name, None)
            .await?
            .map(additional_context),
        // Only humans may keep an agent from stopping; agent chatter waits for the next turn.
        "Stop" => deliver(backend, &me.name, name, Some(proto::Kind::Human))
            .await?
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

/// Take the agent's unread mail (optionally only from `from_kind`) as hook text.
async fn deliver(
    backend: &mut Backend,
    reader: &str,
    event: &str,
    from_kind: Option<proto::Kind>,
) -> Result<Option<String>> {
    let req = proto::ReadRequest {
        from_kind: from_kind.map(Into::into),
        ..Default::default()
    };
    let msgs = backend.read(req).await?.messages;
    if msgs.is_empty() {
        return Ok(None);
    }
    tracing::info!(reader, count = msgs.len(), event, "delivered via hook");
    Ok(Some(format!(
        "New BBS messages for {reader}:\n\n{}",
        format_messages(&msgs)?
    )))
}

/// When the session runs `bbs register`, bind the session to the new agent.
async fn bind_on_register(
    backend: &mut Backend,
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
    backend.bind_session(req).await?;
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
