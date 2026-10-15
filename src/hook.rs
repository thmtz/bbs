//! Harness hook handlers. Hooks bind a session to the agent it registers and
//! restore that identity after compaction; mail is delivered only by
//! `read --wait`. A hook must never break the harness session, so every failure
//! is logged and swallowed, and unbound sessions are a silent no-op.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::app::identity_line;
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
    let event: Value = serde_json::from_str(&input).context("parsing hook input")?;
    let Some(session_id) = event["session_id"].as_str() else {
        return Ok(());
    };
    let harness = harness.as_str();
    // Claude Code and Codex share a hook protocol: the same event names, `Bash`
    // tool events, and `additionalContext` injection. Other events, including
    // ones registered by older installs, return before connecting.
    match (
        event["hook_event_name"].as_str().unwrap_or_default(),
        event["source"].as_str(),
    ) {
        ("PostToolUse", _) => {
            if let Some(token) = registered_token(&event) {
                let req = proto::BindSessionRequest {
                    harness: harness.to_string(),
                    session_id: session_id.to_string(),
                    token,
                };
                if let Some(mut backend) = open(dir).await? {
                    backend.bind_session(req).await?;
                }
            }
        }
        ("SessionStart", Some("compact")) => {
            let Some(mut backend) = open(dir).await? else {
                return Ok(());
            };
            let req = proto::SessionAgentRequest {
                harness: harness.to_string(),
                session_id: session_id.to_string(),
            };
            let bound = backend.session_agent(req).await?;
            if let Some(me) = bound.agent {
                let text = format!(
                    "You are registered on the BBS. {}",
                    identity_line(&me.name, &bound.token)
                );
                let output = json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": text } });
                println!("{output}");
            }
        }
        _ => {}
    }
    Ok(())
}

/// The configured backend, or `None` when there is no board to bind to.
async fn open(dir: &Path) -> Result<Option<Backend>> {
    let config = Config::load()?;
    if config.server.is_none() && !dir.join("bbs.db").exists() {
        return Ok(None);
    }
    // A down server fails within the connect timeout, so it never stalls the harness.
    Ok(Some(Backend::open(&config, dir, Caller::default()).await?))
}

/// The new agent's token when the session just ran `bbs register`.
fn registered_token(event: &Value) -> Option<String> {
    if event["tool_name"].as_str() != Some("Bash") {
        return None;
    }
    let command = event["tool_input"]["command"].as_str().unwrap_or_default();
    if !(command.contains("bbs") && command.contains("register")) {
        return None;
    }
    // Claude Code reports `{stdout, stderr, ...}`; Codex reports the output as a string.
    let response = &event["tool_response"];
    let stdout = response["stdout"]
        .as_str()
        .or(response.as_str())
        .unwrap_or_default();
    parse_welcome_token(stdout)
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
