use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

/// Which backend the CLI under test uses.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// The CLI opens ~/.local/state/bbs/bbs.db itself.
    Local,
    /// The CLI talks to a `bbs serve` spawned for the test, via `BBS_SERVER`.
    Remote,
}

/// Generate `local::<test>` and `remote::<test>` for each `fn <test>(mode: Mode)`.
macro_rules! both_modes {
    ($($name:ident),* $(,)?) => {
        mod local {
            $(#[test] fn $name() { super::$name(super::Mode::Local) })*
        }
        mod remote {
            $(#[test] fn $name() { super::$name(super::Mode::Remote) })*
        }
    };
}

struct Server {
    child: Child,
    addr: String,
    state: PathBuf,
}

impl Server {
    fn start(home: &PathBuf) -> Server {
        let state = home.join("server-state");
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0", "--state-dir"])
            .arg(&state)
            .env("HOME", home)
            .env_remove("BBS_SERVER")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .strip_prefix("bbs serve listening on ")
            .unwrap_or_else(|| panic!("unexpected serve output: {line:?}"))
            .to_string();
        Server { child, addr, state }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Bbs {
    home: PathBuf,
    server: Option<Server>,
}

impl Bbs {
    fn new(test: &str) -> Bbs {
        Bbs::with_mode(test, Mode::Local)
    }

    fn with_mode(test: &str, mode: Mode) -> Bbs {
        let home =
            std::env::temp_dir().join(format!("bbs-test-{}-{test}-{mode:?}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let server = match mode {
            Mode::Local => None,
            Mode::Remote => Some(Server::start(&home)),
        };
        let bbs = Bbs { home, server };
        bbs.ok(&["init"]);
        bbs
    }

    /// A `bbs` invocation as the human "will", pointed at this test's backend.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bbs"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env("USER", "will")
            .env_remove("BBS_SERVER");
        if let Some(server) = &self.server {
            cmd.env("BBS_SERVER", &server.addr);
        }
        cmd
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        self.run_with_env(args, stdin, &[])
    }

    fn run_with_env(&self, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
        let mut child = self
            .command(args)
            .envs(env.iter().copied())
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
        self.register_with(&["register", "--note", "testing"])
    }

    fn register_with(&self, args: &[&str]) -> (String, String) {
        let out = self.ok(args);
        let name = out.split('"').nth(1).unwrap().to_string();
        let token = out
            .split("--token ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(token.len(), 26, "{out}");
        (name, token)
    }

    /// The backend's database: the server's in remote mode.
    fn db(&self) -> rusqlite::Connection {
        let state = match &self.server {
            Some(server) => server.state.clone(),
            None => self.home.join(".local/state/bbs"),
        };
        rusqlite::Connection::open(state.join("bbs.db")).unwrap()
    }
}

impl Drop for Bbs {
    fn drop(&mut self) {
        // Stop the server before removing its state directory.
        self.server = None;
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
        .env_remove("BBS_SERVER")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bbs init"));
    std::fs::remove_dir_all(&home).unwrap();
}

fn init_is_idempotent(mode: Mode) {
    let bbs = Bbs::with_mode("reinit", mode);
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

fn requested_names_are_validated_and_reserved(mode: Mode) {
    let bbs = Bbs::with_mode("requested-name", mode);
    let (name, token) = bbs.register_with(&["register", "--name", "Jeffrey"]);
    assert_eq!(name, "Jeffrey");
    assert_eq!(bbs.ok(&["--token", &token, "whoami"]), "Jeffrey\n");
    assert!(
        bbs.err(&["register", "--name", "jEfFrEy"])
            .contains("already reserved")
    );
    assert!(
        bbs.err(&["register", "--name", "WILL"])
            .contains("already reserved")
    );
    assert!(
        bbs.err(&["human", "add", "Jeffrey"])
            .contains("in use by an agent")
    );
    assert!(
        bbs.err(&["--as", "missing", "register"])
            .contains("unknown human")
    );
    for invalid in [
        "",
        "@Jeffrey",
        "Jeffrey@will",
        "two words",
        "a,b",
        "../x",
        "-a",
        "é",
        &"a".repeat(65),
    ] {
        assert!(
            bbs.err(&["register", &format!("--name={invalid}")])
                .contains("invalid agent name")
        );
    }
    let count: i64 = bbs
        .db()
        .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "failed requests must not register fallback identities"
    );
    bbs.ok(&["--token", &token, "bye"]);
    let (name, replacement) = bbs.register_with(&["register", "--name", "Jeffrey"]);
    assert_eq!(name, "Jeffrey");
    assert_ne!(replacement, token);
    bbs.register_with(&["register", "--name", "Agent_1.test-2"]);
}

fn concurrent_registration_reserves_one_name(mode: Mode) {
    let bbs = Bbs::with_mode("name-race", mode);
    let children: Vec<_> = (0..6)
        .map(|_| bbs.spawn(&["register", "--name", "Jeffrey"]))
        .collect();
    let mut successes = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        if out.status.success() {
            successes += 1;
        } else {
            let err = String::from_utf8(out.stderr).unwrap();
            assert!(err.contains("already reserved"), "{err}");
        }
    }
    assert_eq!(successes, 1);
}

fn registration_records_human_from_as_config_or_user(mode: Mode) {
    let bbs = Bbs::with_mode("owner-config", mode);
    let (_, user) = bbs.register();
    bbs.ok(&["init", "--human", "Ada"]);
    let (_, configured) = bbs.register();
    let (_, explicit) = bbs.register_with(&["--as", "WILL", "register"]);
    let db = bbs.db();
    for (token, expected) in [(&user, "will"), (&configured, "Ada"), (&explicit, "will")] {
        let owner: String = db
            .query_row(
                "SELECT h.name FROM agents a JOIN agent_owners o ON o.agent_id = a.id
             JOIN humans h ON h.id = o.human_id WHERE a.token = ?1",
                [token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, expected);
    }
    assert!(
        db.execute(
            "INSERT INTO agent_owners (agent_id, human_id)
         SELECT a.id, h.id FROM agents a, humans h WHERE a.token = ?1 AND h.name = 'Ada'",
            [&user],
        )
        .is_err(),
        "an agent may have only one owner"
    );
}

fn cross_owner_messages_are_qualified_for_each_reader(mode: Mode) {
    let bbs = Bbs::with_mode("cross-owner", mode);
    bbs.ok(&["human", "add", "Ada"]);
    bbs.ok(&["group", "register", "acme"]);
    let (sender, ts) = bbs.register_with(&["register", "--name", "Jeffrey"]);
    let (same, same_token) = bbs.register();
    let (other, other_token) = bbs.register_with(&["--as", "ada", "register"]);
    for token in [&ts, &same_token, &other_token] {
        bbs.ok(&["--token", token, "group", "join", "acme"]);
    }
    for token in [&ts, &same_token, &other_token] {
        bbs.ok(&["--token", token, "read"]);
    }
    bbs.ok(&["--token", &ts, "send", "--to", "@acme", "broadcast"]);
    let same_mail = bbs.ok(&["--token", &same_token, "read"]);
    let other_mail = bbs.ok(&["--token", &other_token, "read"]);
    assert!(same_mail.contains("from Jeffrey to @acme:"), "{same_mail}");
    assert!(
        other_mail.contains("from Jeffrey@will to @acme:"),
        "{other_mail}"
    );
    assert!(bbs.ok(&["read"]).contains("from Jeffrey to @acme:"));

    bbs.ok(&["--token", &ts, "send", "--to", &other, "direct"]);
    let direct = bbs.ok(&["--token", &other_token, "read", "--wait"]);
    assert!(
        direct.contains(&format!("from Jeffrey@will to {other}:\ndirect")),
        "{direct}"
    );
    bbs.ok(&[
        "--token",
        &other_token,
        "send",
        "--to",
        "jeffrey@WILL",
        "reply",
    ]);
    let reply = bbs.ok(&["--token", &ts, "read"]);
    assert!(
        reply.contains(&format!("from {other}@Ada to Jeffrey:")),
        "{reply}"
    );
    assert!(
        bbs.err(&[
            "--token",
            &other_token,
            "send",
            "--to",
            "Jeffrey@Ada",
            "wrong owner"
        ])
        .contains("no such")
    );
    bbs.ok(&["--token", &ts, "send", "--to", &same, "same owner"]);
    assert!(
        bbs.ok(&["--token", &same_token, "read"])
            .contains("from Jeffrey to ")
    );

    bbs.ok(&["--token", &ts, "bye"]);
    assert!(
        bbs.err(&[
            "--token",
            &other_token,
            "send",
            "--to",
            "Jeffrey@will",
            "retired"
        ])
        .contains("offline")
    );
    let (_, replacement) = bbs.register_with(&["--as", "Ada", "register", "--name", &sender]);
    bbs.ok(&["--token", &replacement, "group", "join", "acme"]);
    bbs.ok(&["--token", &replacement, "send", "--to", &other, "new owner"]);
    assert!(
        bbs.ok(&["--token", &other_token, "read"])
            .contains(&format!("from Jeffrey to {other}:\nnew owner"))
    );
    let history = bbs.ok(&["--token", &other_token, "read", "--archived"]);
    assert!(
        history.contains(&format!("from Jeffrey@will to {other}:\ndirect")),
        "{history}"
    );
    assert!(
        bbs.err(&[
            "--token",
            &other_token,
            "send",
            "--to",
            "Jeffrey@will",
            "stale address"
        ])
        .contains("offline")
    );
}

#[test]
fn online_backup_preserves_committed_state_and_restores() {
    let bbs = Bbs::with_mode("backup", Mode::Remote);
    let (name, token) = bbs.register();
    bbs.ok(&["send", "--to", &name, "saved message"]);
    let state = &bbs.server.as_ref().unwrap().state;
    let destination = bbs.home.join("snapshot.db");
    let mut source = bbs.db();
    let transaction = source.transaction().unwrap();
    transaction
        .execute(
            "UPDATE humans SET name = 'uncommitted' WHERE name = 'will'",
            [],
        )
        .unwrap();
    let output = bbs
        .command(&[
            "backup",
            destination.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .env("BBS_SERVER", "127.0.0.1:1")
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    transaction.rollback().unwrap();
    let snapshot = rusqlite::Connection::open(&destination).unwrap();
    let human: String = snapshot
        .query_row("SELECT name FROM humans", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        human, "will",
        "uncommitted changes must not enter the backup"
    );
    let integrity: String = snapshot
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let journal: String = snapshot
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete", "backup must be a standalone file");
    drop(snapshot);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!destination.with_file_name("snapshot.db-wal").exists());
    // The live server still works; changes after the snapshot stay out of it.
    assert!(
        bbs.ok(&["--token", &token, "read"])
            .contains("saved message")
    );
    bbs.ok(&["send", "--to", &name, "after backup"]);
    let restored_home = bbs.home.join("restored");
    std::fs::create_dir_all(restored_home.join("server-state")).unwrap();
    std::fs::copy(&destination, restored_home.join("server-state/bbs.db")).unwrap();
    let restored = Bbs {
        server: Some(Server::start(&restored_home)),
        home: restored_home,
    };
    let mail = restored.ok(&["--token", &token, "read"]);
    assert!(mail.contains("saved message"));
    assert!(!mail.contains("after backup"));
}

#[test]
fn backup_refuses_existing_files_and_cleans_up_failures() {
    let bbs = Bbs::new("backup-errors");
    let destination = bbs.home.join("snapshot.db");
    std::fs::write(&destination, "keep me").unwrap();
    assert!(
        bbs.err(&["backup", destination.to_str().unwrap()])
            .contains("already exists")
    );
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "keep me");
    std::fs::remove_file(&destination).unwrap();
    let missing = bbs.home.join("missing-state");
    assert!(
        bbs.err(&[
            "backup",
            destination.to_str().unwrap(),
            "--state-dir",
            missing.to_str().unwrap()
        ])
        .contains("backup source")
    );
    assert!(!destination.exists());
    assert!(!missing.exists());
    let sidecar = bbs.home.join("snapshot.db-wal");
    std::fs::write(&sidecar, "keep me").unwrap();
    assert!(
        bbs.err(&["backup", destination.to_str().unwrap()])
            .contains("sidecar")
    );
    assert_eq!(std::fs::read_to_string(sidecar).unwrap(), "keep me");
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(&bbs.home).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp")
    }));
}

#[test]
fn backup_pins_snapshot_across_copy_steps() {
    let bbs = Bbs::with_mode("backup-snapshot", Mode::Remote);
    let (name, _) = bbs.register();
    bbs.ok(&["send", "--to", &name, "before copy"]);
    bbs.db()
        .execute_batch(
            "CREATE TABLE padding (data BLOB); INSERT INTO padding VALUES (zeroblob(16777216))",
        )
        .unwrap();
    let destination = bbs.home.join("snapshot.db");
    let child = bbs.spawn(&[
        "backup",
        destination.to_str().unwrap(),
        "--state-dir",
        bbs.server.as_ref().unwrap().state.to_str().unwrap(),
    ]);
    // A partly written temporary database establishes that copying has begun,
    // without relying on a sleep to guess when the snapshot was acquired.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let copying = std::fs::read_dir(&bbs.home).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry.file_name().to_string_lossy().starts_with(".tmp")
                && !entry.file_name().to_string_lossy().ends_with("-journal")
                && entry.metadata().unwrap().len() > 1024 * 1024
        });
        if copying {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backup did not begin copying pages"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    bbs.ok(&["send", "--to", &name, "during copy"]);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snapshot = rusqlite::Connection::open(&destination).unwrap();
    let count: i64 = snapshot
        .query_row(
            "SELECT count(*) FROM messages WHERE body = 'during copy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "writes during copying must not restart the snapshot"
    );
    let count: i64 = snapshot
        .query_row(
            "SELECT count(*) FROM messages WHERE body = 'before copy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn backup_refuses_source_sidecar_names() {
    let bbs = Bbs::new("backup-sidecar");
    let state = bbs.home.join(".local/state/bbs");
    for suffix in ["-wal", "-shm", "-journal"] {
        let destination = state.join(format!("bbs.db{suffix}"));
        let error = bbs.err(&["backup", destination.to_str().unwrap()]);
        assert!(
            error.contains("sidecar") || error.contains("already exists"),
            "{error}"
        );
    }
    #[cfg(unix)]
    {
        let alias = bbs.home.join("state-alias");
        std::os::unix::fs::symlink(&state, &alias).unwrap();
        let destination = alias.join("bbs.db-journal");
        assert!(
            bbs.err(&["backup", destination.to_str().unwrap()])
                .contains("source database sidecar")
        );
    }
}

fn config_path(bbs: &Bbs) -> PathBuf {
    bbs.home.join(".config/bbs/config.toml")
}

#[test]
fn init_writes_and_merges_config() {
    let bbs = Bbs::new("config");
    assert!(!config_path(&bbs).exists(), "plain init writes no config");
    // Bind a session that has human mail waiting, so a Stop hook would block.
    let register_out = bbs.ok(&["register"]);
    let name = register_out.split('"').nth(1).unwrap();
    hook(
        &bbs,
        serde_json::json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "bbs register" },
            "tool_response": { "stdout": register_out, "stderr": "" },
        }),
    );
    bbs.ok(&["send", "--to", name, "hello"]);

    // Failed enrollment preserves the working local configuration.
    let down = "connecting to bbs server at 127.0.0.1:1";
    assert!(bbs.err(&["init", "--server", "127.0.0.1:1"]).contains(down));
    assert!(!config_path(&bbs).exists());
    bbs.ok(&["init", "--human", "ada"]);
    let config = std::fs::read_to_string(config_path(&bbs)).unwrap();
    assert!(config.contains("human = \"ada\""), "{config}");
    assert!(!config.contains("server"));
    std::fs::write(
        config_path(&bbs),
        format!("{config}server = \"127.0.0.1:1\"\n"),
    )
    .unwrap();

    // Commands use the configured server; hooks stay silent when it is down.
    assert!(bbs.err(&["whoami"]).contains(down));
    let stop = serde_json::json!({"session_id": "s1", "hook_event_name": "Stop"});
    assert_eq!(hook(&bbs, stop.clone()), None);
    std::fs::remove_file(config_path(&bbs)).unwrap();
    assert_eq!(hook(&bbs, stop).unwrap()["decision"], "block");
}

#[test]
fn human_name_comes_from_as_then_config_then_user() {
    let bbs = Bbs::new("human-config");
    // Local init with --human saves the name and reserves it.
    let out = bbs.ok(&["init", "--human", "ada"]);
    assert!(out.contains("Human: ada"), "{out}");
    assert_eq!(bbs.ok(&["whoami"]), "ada\n");
    assert_eq!(bbs.ok(&["--as", "will", "whoami"]), "will\n");
    std::fs::remove_file(config_path(&bbs)).unwrap();
    assert_eq!(bbs.ok(&["whoami"]), "will\n");
}

#[test]
fn bbs_server_env_overrides_config() {
    let bbs = Bbs::new("server-env");
    let out = bbs.run_with_env(&["whoami"], "", &[("BBS_SERVER", "127.0.0.1:1")]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("connecting to bbs server at 127.0.0.1:1")
    );
    assert!(!config_path(&bbs).exists());
}

#[test]
fn remote_backend_round_trip() {
    let bbs = Bbs::new("remote");
    let server = Server::start(&bbs.home);
    let out = bbs.ok(&["init", "--server", &server.addr]);
    assert!(out.contains("Human: will"), "{out}");

    bbs.ok(&["group", "register", "acme"]);
    let register_out = bbs.ok(&["register"]);
    let (a, ta) = bbs.register();
    let b = register_out.split('"').nth(1).unwrap();
    bbs.ok(&["--token", &ta, "group", "join", "acme"]);
    assert!(
        bbs.err(&["--token", &ta, "send", "--to", "will", "hi"])
            .contains("cannot message humans")
    );
    bbs.ok(&["--token", &ta, "send", "--to", "@acme", "standup"]);
    assert!(bbs.ok(&["read"]).contains("standup"));
    assert_eq!(bbs.ok(&["whoami"]), "will\n");

    // Hooks bind and deliver through the server.
    let post_tool = serde_json::json!({
        "session_id": "s1",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "bbs register" },
        "tool_response": { "stdout": register_out, "stderr": "" },
    });
    assert!(hook(&bbs, post_tool).is_none());
    bbs.ok(&["send", "--to", b, "from will"]);
    let stop = serde_json::json!({"session_id": "s1", "hook_event_name": "Stop"});
    let out = hook(&bbs, stop).unwrap();
    assert!(
        out["reason"].as_str().unwrap().contains("from will"),
        "{out}"
    );

    // `read --wait` blocks on the server until mail arrives.
    let mut waiter = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        bbs.err(&["--token", &ta, "read", "--wait"])
            .contains("already running")
    );
    bbs.ok(&["send", "--to", &a, "wake up"]);
    assert!(wait_exit(&mut waiter, 5).unwrap().success());
    let out = waiter.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("wake up"));

    // Everything went to the server, not the database from the local `init`.
    let local_agents: i64 = bbs
        .db()
        .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(local_agents, 0);
}

fn direct_messages_within_group(mode: Mode) {
    let bbs = Bbs::with_mode("dm", mode);
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

fn humans_message_agents_and_humans_but_agents_cannot_dm_humans(mode: Mode) {
    let bbs = Bbs::with_mode("human", mode);
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

fn broadcast_reaches_members_and_humans(mode: Mode) {
    let bbs = Bbs::with_mode("broadcast", mode);
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
    // A has only B's join notice, not its own broadcast.
    assert!(!bbs.ok(&["--token", &ta, "read"]).contains("standup"));
    assert_eq!(bbs.ok(&["--token", &tc, "read"]), "");
    assert!(bbs.ok(&["--as", "alice", "read"]).contains("standup"));

    let out = bbs.ok(&["send", "--to", "@acme", "from a human"]);
    assert!(out.contains("3 recipients"), "{out}"); // both agents + alice
    assert!(
        bbs.ok(&["--token", &ta, "read"])
            .contains("from will (human) to @acme")
    );
}

fn bye_retires_token_and_name(mode: Mode) {
    let bbs = Bbs::with_mode("bye", mode);
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

fn bye_notifies_and_leaves_groups(mode: Mode) {
    let bbs = Bbs::with_mode("bye-groups", mode);
    bbs.ok(&["group", "register", "g"]);
    bbs.ok(&["group", "register", "h"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let (c, tc) = bbs.register();
    for g in ["g", "h"] {
        bbs.ok(&["--token", &ta, "group", "join", g]);
    }
    bbs.ok(&["--token", &tb, "group", "join", "g"]);
    bbs.ok(&["--token", &tc, "group", "join", "h"]);
    for t in [&tb, &tc] {
        bbs.ok(&["--token", t, "read"]);
    }
    bbs.ok(&["read"]);

    bbs.ok(&["--token", &ta, "bye"]);
    let from_g = format!("from {a} to @g:\n{a} retired and left @g.\n");
    let from_h = format!("from {a} to @h:\n{a} retired and left @h.\n");
    let mail_b = bbs.ok(&["--token", &tb, "read"]);
    assert!(
        mail_b.contains(&from_g) && !mail_b.contains("@h"),
        "{mail_b}"
    );
    let mail_c = bbs.ok(&["--token", &tc, "read"]);
    assert!(
        mail_c.contains(&from_h) && !mail_c.contains("@g"),
        "{mail_c}"
    );
    let human = bbs.ok(&["read"]);
    assert!(
        human.contains(&from_g) && human.contains(&from_h),
        "{human}"
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@g: {b}\n@h: {c}\n"));
}

fn humans_retire_agents(mode: Mode) {
    let bbs = Bbs::with_mode("retire", mode);
    bbs.ok(&["human", "add", "ada"]);
    bbs.ok(&["group", "register", "g"]);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    for t in [&ta, &tb] {
        bbs.ok(&["--token", t, "group", "join", "g"]);
    }
    bbs.ok(&["--token", &ta, "read"]);
    bbs.ok(&["--as", "ada", "read"]);

    assert!(
        bbs.err(&["--token", &tb, "retire", &a])
            .contains("humans only")
    );
    assert_eq!(bbs.ok(&["retire", &a]), format!("Retired {a}.\n"));
    let notice = format!("from will (human) to @g:\nwill retired {a} and left @g.\n");
    assert!(bbs.ok(&["--token", &tb, "read"]).contains(&notice));
    assert!(bbs.ok(&["--as", "ada", "read"]).contains(&notice));
    assert!(
        bbs.err(&["--token", &ta, "read"])
            .contains("unknown or retired token")
    );
    assert_eq!(bbs.ok(&["group", "list"]), format!("@g: {b}\n"));
    assert!(bbs.err(&["retire", &a]).contains("no live agent"));
}

fn stale_agents_are_retired_on_register(mode: Mode) {
    let bbs = Bbs::with_mode("stale", mode);
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

fn human_name_is_reserved_and_permissions_enforced(mode: Mode) {
    let bbs = Bbs::with_mode("perms", mode);
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

fn whoami_reports_agent_or_human(mode: Mode) {
    let bbs = Bbs::with_mode("whoami", mode);
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

fn humans_remove_agents_from_groups(mode: Mode) {
    let bbs = Bbs::with_mode("remove", mode);
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

fn joining_notifies_the_group(mode: Mode) {
    let bbs = Bbs::with_mode("join", mode);
    bbs.ok(&["group", "register", "g"]);
    let (_a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    bbs.ok(&["--token", &ta, "group", "join", "g"]);
    bbs.ok(&["--token", &tb, "group", "join", "g"]);

    let joined = format!("from {b} to @g:\n{b} joined @g.\n");
    assert!(bbs.ok(&["--token", &ta, "read"]).contains(&joined));
    assert!(bbs.ok(&["read"]).contains(&joined));
    assert_eq!(bbs.ok(&["--token", &tb, "read"]), "");

    // Re-joining is a no-op and announces nothing.
    assert!(
        bbs.ok(&["--token", &tb, "group", "join", "g"])
            .contains("Already in @g.")
    );
    assert_eq!(bbs.ok(&["--token", &ta, "read"]), "");
    assert_eq!(bbs.ok(&["read"]), "");
}

fn leaving_or_removal_notifies_the_group(mode: Mode) {
    let bbs = Bbs::with_mode("depart", mode);
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

fn agents_lists_status_and_scopes_visibility(mode: Mode) {
    let bbs = Bbs::with_mode("agents", mode);
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

fn active_at_writes_are_debounced(mode: Mode) {
    let bbs = Bbs::with_mode("debounce", mode);
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

fn claude_code_hooks_bind_and_deliver(mode: Mode) {
    let bbs = Bbs::with_mode("hook", mode);
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
    bbs.ok(&["human", "add", "ada"]);
    let (b, tb) = bbs.register_with(&["--as", "ada", "register", "--name", "Jeffrey"]);
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
            .contains(&format!("from {b}@ada to {a}:"))
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

fn codex_hooks_bind_from_string_tool_response(mode: Mode) {
    let bbs = Bbs::with_mode("codex", mode);
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
        self.command(args)
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

fn read_wait_blocks_until_mail_arrives(mode: Mode) {
    let bbs = Bbs::with_mode("wait", mode);
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

fn only_one_read_wait_per_inbox(mode: Mode) {
    let bbs = Bbs::with_mode("wait-lock", mode);
    let (a, ta) = bbs.register();
    let (b, tb) = bbs.register();
    let mut first = bbs.spawn(&["--token", &ta, "read", "--wait"]);
    std::thread::sleep(std::time::Duration::from_millis(500));

    // A second waiter fails fast; other inboxes are unaffected. Try it before
    // sending, since a waiter that gets mail exits and frees the inbox at once.
    let err = bbs.err(&["--token", &ta, "read", "--wait"]);
    assert!(err.contains("already running"), "{err}");
    let mut other = bbs.spawn(&["--token", &tb, "read", "--wait"]);
    bbs.ok(&["send", "--to", &a, "for the first waiter"]);

    let status = wait_exit(&mut first, 5).expect("first waiter did not wake");
    assert!(status.success());
    let out = String::from_utf8(first.wait_with_output().unwrap().stdout).unwrap();
    assert!(out.contains("for the first waiter"), "{out}");
    assert!(
        other.try_wait().unwrap().is_none(),
        "other inbox's waiter exited"
    );
    other.kill().unwrap();
    other.wait().unwrap();

    // The lock is released on exit, even when the waiter is killed.
    bbs.ok(&["send", "--to", &a, "again"]);
    assert!(
        bbs.ok(&["--token", &ta, "read", "--wait"])
            .contains("again")
    );
    bbs.ok(&["send", "--to", &b, "after kill"]);
    assert!(
        bbs.ok(&["--token", &tb, "read", "--wait"])
            .contains("after kill")
    );
}

fn read_wait_fails_if_retired_while_waiting(mode: Mode) {
    let bbs = Bbs::with_mode("wait-retired", mode);
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
                .env_remove("BBS_SERVER")
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
    assert!(version >= 4, "schema still at v{version}");
    std::fs::remove_dir_all(&home).unwrap();
}

fn view_is_for_humans_only(mode: Mode) {
    let bbs = Bbs::with_mode("view", mode);
    let (_a, ta) = bbs.register();
    assert!(bbs.err(&["--token", &ta, "view"]).contains("humans only"));
}

both_modes!(
    requested_names_are_validated_and_reserved,
    concurrent_registration_reserves_one_name,
    registration_records_human_from_as_config_or_user,
    cross_owner_messages_are_qualified_for_each_reader,
    init_is_idempotent,
    direct_messages_within_group,
    humans_message_agents_and_humans_but_agents_cannot_dm_humans,
    broadcast_reaches_members_and_humans,
    bye_retires_token_and_name,
    bye_notifies_and_leaves_groups,
    humans_retire_agents,
    stale_agents_are_retired_on_register,
    human_name_is_reserved_and_permissions_enforced,
    whoami_reports_agent_or_human,
    humans_remove_agents_from_groups,
    joining_notifies_the_group,
    leaving_or_removal_notifies_the_group,
    agents_lists_status_and_scopes_visibility,
    active_at_writes_are_debounced,
    claude_code_hooks_bind_and_deliver,
    codex_hooks_bind_from_string_tool_response,
    read_wait_blocks_until_mail_arrives,
    only_one_read_wait_per_inbox,
    read_wait_fails_if_retired_while_waiting,
    view_is_for_humans_only,
);
