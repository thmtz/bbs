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
