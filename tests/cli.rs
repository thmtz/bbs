use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Bbs {
    home: PathBuf,
}

impl Bbs {
    fn new(test: &str) -> Bbs {
        let home = std::env::temp_dir().join(format!("bbs-test-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let bbs = Bbs { home };
        bbs.ok(&["init"]);
        bbs
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(args)
            .env("HOME", &self.home)
            .env("USER", "will")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, "")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "bbs {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "bbs {args:?} unexpectedly succeeded");
        String::from_utf8(out.stderr).unwrap()
    }

    /// Register an agent and return (name, token).
    fn register(&self) -> (String, String) {
        let out = self.ok(&["register", "--note", "testing"]);
        let name = out.split('"').nth(1).unwrap().to_string();
        let token = out
            .split("--token ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(token.len(), 7, "{out}");
        (name, token)
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home.join(".local/state/bbs/bbs.db")).unwrap()
    }
}

impl Drop for Bbs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn requires_init() {
    let home = std::env::temp_dir().join(format!("bbs-test-{}-noinit", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_bbs"))
        .args(["read"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bbs init"));
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn init_is_idempotent() {
    let bbs = Bbs::new("reinit");
    bbs.ok(&["group", "register", "acme"]);
    let (_, token) = bbs.register();

    let out = bbs.ok(&["init"]);
    assert!(out.contains("Human: will"), "{out}");

    let db = bbs.db();
    let humans: i64 = db
        .query_row("SELECT COUNT(*) FROM humans", [], |r| r.get(0))
        .unwrap();
    assert_eq!(humans, 1);
    assert!(bbs.ok(&["group", "list"]).contains("acme"));
    bbs.ok(&["--token", &token, "group", "join", "acme"]);
}

#[test]
fn direct_messages_within_group() {
    let bbs = Bbs::new("dm");
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    assert_ne!(a, b);

    // Not sharing a group yet.
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", &b, "hi"])
            .contains("not in any of your groups")
    );

    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    bbs.ok(&["--token", &ta, "send", "--to", &b, "hello there"]);

    let out = bbs.ok(&["--token", &tb, "read"]);
    assert!(
        out.contains(&format!("from {a} to {b}:\nhello there\n")),
        "{out}"
    );
    // Archived after reading: empty stdout, exit 0.
    assert_eq!(bbs.ok(&["--token", &tb, "read"]), "");
    let out = bbs.ok(&["--token", &tb, "read", "--archived", "--since", "1h"]);
    assert!(out.contains("hello there"), "{out}");
}

#[test]
fn humans_message_agents_and_humans_but_agents_cannot_dm_humans() {
    let bbs = Bbs::new("human");
    bbs.ok(&["human", "add", "alice"]);
    let (a, ta) = bbs.register();
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "will", "need input"])
            .contains("through your harness")
    );
    bbs.ok(&["send", "--to", "alice", "lunch?"]);
    assert!(
        bbs.ok(&["--as", "alice", "read"])
            .contains("from will (human) to alice:\nlunch?")
    );

    assert!(bbs.err(&["send", "--to", &a, "-"]).contains("empty"));
    let out = bbs.run_with_stdin(&["send", "--to", &a], "multi\nline\n");
    assert!(out.status.success());
    let out = bbs.ok(&["--token", &ta, "read"]);
    assert!(out.contains("from will (human)"), "{out}");
    assert!(out.contains("multi\nline\n"), "{out}");
}

#[test]
fn broadcast_reaches_members_and_humans() {
    let bbs = Bbs::new("broadcast");
    bbs.ok(&["group", "register", "acme"]);
    bbs.ok(&["human", "add", "alice"]);
    let (a, ta) = bbs.register();
    let (_b, tb) = bbs.register();
    let (_c, tc) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);

    assert!(
        bbs.err(&["--token", &tc, "send", "--to", "@acme", "x"])
            .contains("not a member")
    );
    let out = bbs.ok(&["--token", &ta, "send", "--to", "@acme", "standup"]);
    assert!(out.contains("1 recipients"), "{out}"); // humans are not counted for agents

    assert!(
        bbs.ok(&["--token", &tb, "read"])
            .contains(&format!("from {a} to @acme:\nstandup"))
    );
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert!(bbs.ok(&["--as", "alice", "read"]).contains("standup"));

    let out = bbs.ok(&["send", "--to", "@acme", "from a human"]);
    assert!(out.contains("3 recipients"), "{out}"); // both agents + alice
    assert!(
        bbs.ok(&["--token", &ta, "read"])
            .contains("from will (human) to @acme")
    );
}

#[test]
fn bye_retires_token_and_name() {
    let bbs = Bbs::new("bye");
    let (a, ta) = bbs.register();
    bbs.ok(&["send", "--to", &a, "unread"]);
    bbs.ok(&["--token", &ta, "bye"]);
    assert!(
        bbs.err(&["--token", &ta, "read"])
            .contains("unknown or retired token")
    );
    assert!(bbs.err(&["send", "--to", &a, "hi"]).contains("offline"));
    assert!(!bbs.ok(&["agents"]).contains(&a));
    assert!(bbs.ok(&["agents", "--all"]).contains("retired"));
}

#[test]
fn stale_agents_are_retired_on_register() {
    let bbs = Bbs::new("stale");
    let (a, ta) = bbs.register();
    bbs.ok(&["send", "--to", &a, "old news"]);
    bbs.db()
        .execute(
            "UPDATE agents SET active_at = '2000-01-01T00:00:00.000000Z'",
            [],
        )
        .unwrap();
    bbs.register();
    assert!(
        bbs.err(&["--token", &ta, "read"])
            .contains("unknown or retired token")
    );
    let unread: i64 = bbs
        .db()
        .query_row(
            "SELECT COUNT(*) FROM deliveries WHERE archived_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unread, 0);
}

#[test]
fn human_name_is_reserved_and_permissions_enforced() {
    let bbs = Bbs::new("perms");
    let (_a, ta) = bbs.register();
    assert!(
        bbs.err(&["--token", &ta, "group", "register", "x"])
            .contains("humans only")
    );
    assert!(
        bbs.err(&["group", "join", "x"])
            .contains("requires --token")
    );
    assert!(
        bbs.err(&["--as", "nobody", "read"])
            .contains("unknown human")
    );
    let taken: i64 = bbs
        .db()
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE lower(name) = 'will'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(taken, 0);
}

#[test]
fn whoami_reports_agent_or_human() {
    let bbs = Bbs::new("whoami");
    let (a, ta) = bbs.register();
    assert_eq!(bbs.ok(&["--token", &ta, "whoami"]), format!("{a}\n"));
    assert_eq!(bbs.ok(&["whoami"]), "will\n");
    bbs.ok(&["human", "add", "ada"]);
    assert_eq!(bbs.ok(&["--as", "ada", "whoami"]), "ada\n");
    assert!(
        bbs.err(&["--as", "nobody", "whoami"])
            .contains("unknown human")
    );
    bbs.ok(&["--token", &ta, "bye"]);
    assert!(
        bbs.err(&["--token", &ta, "whoami"])
            .contains("unknown or retired token")
    );
}

#[test]
fn humans_remove_agents_from_groups() {
    let bbs = Bbs::new("remove");
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);

    assert!(
        bbs.err(&["--token", &tb, "group", "remove", "acme", &a])
            .contains("humans only")
    );
    assert_eq!(
        bbs.ok(&["group", "remove", "@acme", &a.to_lowercase()]),
        format!("Removed {a} from @acme.\n")
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@acme: {b}\n"));
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "@acme", "hi"])
            .contains("not a member")
    );
    assert!(
        bbs.err(&["group", "remove", "acme", &a])
            .contains(&format!("{a} is not in @acme"))
    );
    assert!(
        bbs.err(&["group", "remove", "acme", "Nobody"])
            .contains("no live agent")
    );
    assert!(
        bbs.err(&["group", "remove", "nope", &b])
            .contains("no such group")
    );
}

#[test]
fn leaving_or_removal_notifies_the_group() {
    let bbs = Bbs::new("depart");
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, tc) = bbs.register();
    let (_d, td) = bbs.register();
    for t in [&ta, &tb, &tc] {
        bbs.ok(&["--token", t, "group", "join", "acme"]);
    }

    bbs.ok(&["--token", &tc, "group", "leave", "acme"]);
    let left = format!("from {c} to @acme:\n{c} left @acme.\n");
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&left));
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&left));
    assert!(bbs.ok(&["read"]).contains(&left));
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert_eq!(bbs.ok(&["--token", &td, "read"]), "");

    bbs.ok(&["group", "remove", "acme", &a]);
    let removed = format!("from will (human) to @acme:\nwill removed {a} from @acme.\n");
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&removed));
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&removed));
    assert!(bbs.ok(&["--as", "ada", "read"]).contains(&removed));
    assert!(!bbs.ok(&["read"]).contains("removed"));
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert_eq!(bbs.ok(&["group", "list"]), format!("@acme: {b}\n"));
}

#[test]
fn agents_lists_status_and_scopes_visibility() {
    let bbs = Bbs::new("agents");
    bbs.ok(&["group", "register", "acme"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, _tc) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "status", "refactoring db layer"]);

    let seen_by_a = bbs.ok(&["--token", &ta, "agents"]);
    assert!(
        seen_by_a.contains(&b) && seen_by_a.contains("refactoring db layer"),
        "{seen_by_a}"
    );
    assert!(!seen_by_a.contains(&c), "{seen_by_a}");
    assert!(!seen_by_a.contains("will"), "{seen_by_a}");
    let seen_by_human = bbs.ok(&["agents"]);
    assert!(seen_by_human.contains(&a) && seen_by_human.contains(&c));
    assert!(seen_by_human.contains("will"));
}

fn hook(bbs: &Bbs, event: serde_json::Value) -> Option<serde_json::Value> {
    let out = bbs.run_with_stdin(&["hook", "claude-code"], &event.to_string());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    (!stdout.trim().is_empty()).then(|| serde_json::from_str(&stdout).unwrap())
}

#[test]
fn active_at_writes_are_debounced() {
    let bbs = Bbs::new("debounce");
    let register_out = bbs.ok(&["register"]);
    let (_, ta) = bbs.register();
    let post_tool = |command: &str, stdout: &str| {
        serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "tool_response": { "stdout": stdout },
        })
    };
    hook(&bbs, post_tool("bbs register", &register_out));
    bbs.ok(&["whoami"]);
    bbs.ok(&["--token", &ta, "whoami"]);

    // Freshly active callers do not write, so other connections see no change.
    let db = bbs.db();
    let version = || -> i64 {
        db.pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap()
    };
    let before = version();
    bbs.ok(&["whoami"]);
    bbs.ok(&["--token", &ta, "agents"]);
    hook(&bbs, post_tool("ls", ""));
    assert_eq!(version(), before);

    // Stale timestamps are still refreshed.
    db.execute_batch(
        "UPDATE agents SET active_at = '2000-01-01T00:00:00.000000Z';
         UPDATE humans SET active_at = '2000-01-01T00:00:00.000000Z';",
    )
    .unwrap();
    bbs.ok(&["whoami"]);
    bbs.ok(&["--token", &ta, "whoami"]);
    hook(&bbs, post_tool("ls", ""));
    let stale: i64 = db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM agents WHERE active_at < '2001')
                  + (SELECT COUNT(*) FROM humans WHERE active_at < '2001')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stale, 0);
}

#[test]
fn claude_code_hooks_bind_and_deliver() {
    let bbs = Bbs::new("hook");
    bbs.ok(&["group", "register", "acme"]);
    let register_out = bbs.ok(&["register"]);
    let (a, ta) = (
        register_out.split('"').nth(1).unwrap().to_string(),
        register_out
            .split("--token ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string(),
    );
    let (b, tb) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    bbs.ok(&["--token", &tb, "group", "join", "acme"]);
    bbs.ok(&["send", "--to", &a, "before binding"]);

    let post_tool = |command: &str, stdout: &str| {
        serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "tool_response": { "stdout": stdout, "stderr": "", "interrupted": false, "isImage": false },
        })
    };
    let event = |name: &str| serde_json::json!({ "session_id": "s1", "hook_event_name": name });

    // Unbound sessions are a silent no-op.
    assert!(hook(&bbs, event("UserPromptSubmit")).is_none());
    assert!(hook(&bbs, serde_json::json!({"garbage": true})).is_none());

    // Seeing `bbs register` output binds the session and delivers pending mail.
    let out = hook(&bbs, post_tool("bbs register --note x", &register_out)).unwrap();
    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(ctx.contains("before binding"), "{ctx}");
    assert!(hook(&bbs, post_tool("ls", "")).is_none());

    // Stop only blocks for human messages.
    bbs.ok(&["--token", &tb, "send", "--to", &a, "agent chatter"]);
    assert!(hook(&bbs, event("Stop")).is_none());
    bbs.ok(&["send", "--to", &a, "human says stop"]);
    let out = hook(&bbs, event("Stop")).unwrap();
    assert_eq!(out["decision"], "block");
    let reason = out["reason"].as_str().unwrap();
    assert!(
        reason.contains("human says stop") && !reason.contains("agent chatter"),
        "{reason}"
    );

    // The agent message is still waiting for the next prompt.
    let out = hook(&bbs, event("UserPromptSubmit")).unwrap();
    assert!(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(&format!("from {b}"))
    );

    // After compaction the identity is re-injected.
    let out = hook(&bbs, serde_json::json!({ "session_id": "s1", "hook_event_name": "SessionStart", "source": "compact" })).unwrap();
    assert!(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(&ta)
    );

    // Re-registering in the same session retires the old identity.
    let second = bbs.ok(&["register"]);
    hook(&bbs, post_tool("bbs register", &second));
    assert!(bbs.err(&["--token", &ta, "read"]).contains("retired"));
}

#[test]
fn codex_hooks_bind_from_string_tool_response() {
    let bbs = Bbs::new("codex");
    let register_out = bbs.ok(&["register"]);
    let token = register_out
        .split("--token ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let codex = |event: serde_json::Value| {
        let out = bbs.run_with_stdin(&["hook", "codex"], &event.to_string());
        assert!(out.status.success());
        let stdout = String::from_utf8(out.stdout).unwrap();
        (!stdout.trim().is_empty())
            .then(|| serde_json::from_str::<serde_json::Value>(&stdout).unwrap())
    };
    let bind = serde_json::json!({
        "session_id": "c1",
        "turn_id": "t1",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "cd /repo && bbs register --note 'codex'" },
        "tool_response": register_out,
    });
    assert!(codex(bind).is_none());
    bbs.ok(&[
        "send",
        "--to",
        register_out.split('"').nth(1).unwrap(),
        "hi codex",
    ]);

    // Bindings are per harness: the same session id under claude-code is unbound.
    let stop = serde_json::json!({ "session_id": "c1", "hook_event_name": "Stop", "stop_hook_active": false });
    let out = bbs.run_with_stdin(&["hook", "claude-code"], &stop.to_string());
    assert!(out.stdout.is_empty());

    let out = codex(stop).unwrap();
    assert_eq!(out["decision"], "block");
    assert!(out["reason"].as_str().unwrap().contains("hi codex"));
    // Delivered via the hook, so nothing is left unread.
    assert_eq!(bbs.ok(&["--token", token, "read"]), "");
}

impl Bbs {
    fn spawn(&self, args: &[&str]) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(args)
            .env("HOME", &self.home)
            .env("USER", "will")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

/// Wait up to `secs` for the child to exit.
fn wait_exit(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

#[test]
fn read_wait_blocks_until_mail_arrives() {
    let bbs = Bbs::new("wait");
    let (a, ta) = bbs.register();
    let (b, _tb) = bbs.register();
    assert!(
        bbs.err(&["read", "--wait", "--archived"])
            .contains("cannot be used with")
    );

    // Already-unread mail returns immediately.
    bbs.ok(&["send", "--to", &a, "early"]);
    assert!(
        bbs.ok(&["--token", &ta, "read", "--wait"])
            .contains("early")
    );

    let mut child = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(child.try_wait().unwrap().is_none(), "returned without mail");

    // Mail for someone else does not wake it.
    bbs.ok(&["send", "--to", &b, "not for a"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "woke for another inbox"
    );

    let sent = std::time::Instant::now();
    bbs.ok(&["send", "--to", &a, "wake up"]);
    let status = wait_exit(&mut child, 5).expect("read --wait did not wake");
    assert!(status.success());
    assert!(sent.elapsed() < std::time::Duration::from_millis(2500));
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("wake up") && !stdout.contains("not for a"),
        "{stdout}"
    );
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");
}

#[test]
fn read_wait_fails_if_retired_while_waiting() {
    let bbs = Bbs::new("wait-retired");
    let (_a, ta) = bbs.register();
    let mut child = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    bbs.ok(&["--token", &ta, "bye"]);
    let status = wait_exit(&mut child, 5).expect("waiter did not notice retirement");
    assert!(!status.success());
    let err = String::from_utf8(child.wait_with_output().unwrap().stderr).unwrap();
    assert!(err.contains("retired"), "{err}");
}

#[test]
fn old_schema_is_migrated_on_open_even_concurrently() {
    let home = std::env::temp_dir().join(format!("bbs-test-{}-migrate", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let state = home.join(".local/state/bbs");
    std::fs::create_dir_all(&state).unwrap();
    let db = rusqlite::Connection::open(state.join("bbs.db")).unwrap();
    db.execute_batch(include_str!("../migrations/0001_init.sql"))
        .unwrap();
    db.execute_batch(
        "PRAGMA user_version = 1;
         INSERT INTO humans (name, created_at, active_at) VALUES ('will', 'x', 'x');",
    )
    .unwrap();
    drop(db);

    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_bbs"))
                .arg("agents")
                .env("HOME", &home)
                .env("USER", "will")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let db = rusqlite::Connection::open(state.join("bbs.db")).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert!(version >= 3, "schema still at v{version}");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn view_is_for_humans_only() {
    let bbs = Bbs::new("view");
    let (_a, ta) = bbs.register();
    assert!(bbs.err(&["--token", &ta, "view"]).contains("humans only"));
}
