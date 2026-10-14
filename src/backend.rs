//! Where commands run: in-process against the local SQLite file, or over gRPC
//! against a `bbs serve` process. Both take and return the protobuf types, so
//! the CLI formats the same responses either way.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::Connection;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};

use crate::config::Config;
use crate::db;
use crate::proto::{self, bbs_client::BbsClient};
use crate::service::{self, Caller};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const FEED_POLL_INTERVAL: Duration = Duration::from_secs(1);
const WAIT_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub enum Backend {
    Local(Local),
    Remote(Remote),
}

pub struct Local {
    conn: Connection,
    dir: PathBuf,
    caller: Caller,
}

pub struct Remote {
    client: BbsClient<Channel>,
    caller: Caller,
}

impl Local {
    /// Open (creating if needed) and migrate the database in `dir`.
    pub fn open(dir: &Path, caller: Caller) -> Result<Local> {
        let mut conn = db::open(&dir.join("bbs.db"))?;
        db::migrate(&mut conn)?;
        Ok(Local {
            conn,
            dir: dir.to_path_buf(),
            caller,
        })
    }
}

impl Remote {
    /// Connect to `bbs serve` at `addr` (host:port), failing fast if it is down.
    pub async fn connect(addr: &str, caller: Caller) -> Result<Remote> {
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .with_context(|| format!("invalid server address {addr:?}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .connect()
            .await
            .with_context(|| format!("connecting to bbs server at {addr}"))?;
        Ok(Remote {
            client: BbsClient::new(channel),
            caller,
        })
    }

    /// Wrap `msg` in a request carrying the caller's identity.
    fn request<T>(&self, msg: T) -> Result<tonic::Request<T>> {
        let mut req = tonic::Request::new(msg);
        if self.caller.token.is_none()
            && let Some(token) = &self.caller.human_token
        {
            req.metadata_mut().insert(
                "bbs-human-token",
                MetadataValue::try_from(token.as_str()).context("invalid human token metadata")?,
            );
        }
        let (key, value) = match (&self.caller.token, &self.caller.human) {
            (Some(token), _) => ("bbs-token", token),
            (None, Some(human)) => ("bbs-human", human),
            (None, None) => return Ok(req),
        };
        let value = MetadataValue::try_from(value.as_str())
            .with_context(|| format!("invalid {key} metadata"))?;
        req.metadata_mut().insert(key, value);
        Ok(req)
    }
}

/// The server sends the service's error text as the status message.
fn status_error(status: tonic::Status) -> anyhow::Error {
    anyhow!("{}", status.message())
}

/// One unary RPC: `rpc!(remote, method, request)`.
macro_rules! rpc {
    ($remote:expr, $method:ident, $msg:expr) => {{
        let req = $remote.request($msg)?;
        $remote
            .client
            .$method(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(status_error)
    }};
}

impl Backend {
    /// The configured server, else the local database `bbs init` created in `dir`.
    pub async fn open(config: &Config, dir: &Path, mut caller: Caller) -> Result<Backend> {
        if let Some(addr) = &config.server {
            caller.human_token = config.human_token.clone();
            return Ok(Backend::Remote(Remote::connect(addr, caller).await?));
        }
        if !dir.join("bbs.db").exists() {
            bail!("BBS is not initialized; run `bbs init`");
        }
        Ok(Backend::Local(Local::open(dir, caller)?))
    }

    pub fn set_caller(&mut self, caller: Caller) {
        match self {
            Backend::Local(l) => l.caller = caller,
            Backend::Remote(r) => r.caller = caller,
        }
    }

    pub async fn ensure_human(
        &mut self,
        req: proto::EnsureHumanRequest,
    ) -> Result<proto::EnsureHumanResponse> {
        match self {
            Backend::Local(l) => service::ensure_human(&mut l.conn, req),
            Backend::Remote(r) => rpc!(r, ensure_human, req),
        }
    }

    pub async fn add_human(&mut self, req: proto::AddHumanRequest) -> Result<proto::Empty> {
        match self {
            Backend::Local(l) => service::add_human(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, add_human, req),
        }
    }

    pub async fn register(
        &mut self,
        req: proto::RegisterRequest,
    ) -> Result<proto::RegisterResponse> {
        match self {
            Backend::Local(l) => service::register(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, register, req),
        }
    }

    pub async fn whoami(&mut self) -> Result<proto::Principal> {
        match self {
            Backend::Local(l) => service::whoami(&mut l.conn, &l.caller),
            Backend::Remote(r) => rpc!(r, whoami, proto::Empty {}),
        }
    }

    pub async fn server_info(&mut self) -> Result<proto::ServerInfoResponse> {
        match self {
            Backend::Remote(r) => rpc!(r, server_info, proto::Empty {}),
            Backend::Local(_) => Ok(proto::ServerInfoResponse {
                require_human_token: false,
            }),
        }
    }

    pub async fn set_status(&mut self, req: proto::SetStatusRequest) -> Result<proto::Empty> {
        match self {
            Backend::Local(l) => service::set_status(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, set_status, req),
        }
    }

    pub async fn bye(&mut self) -> Result<proto::ByeResponse> {
        match self {
            Backend::Local(l) => service::bye(&mut l.conn, &l.caller),
            Backend::Remote(r) => rpc!(r, bye, proto::Empty {}),
        }
    }

    pub async fn retire(&mut self, req: proto::RetireRequest) -> Result<proto::ByeResponse> {
        match self {
            Backend::Local(l) => service::retire_agent(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, retire, req),
        }
    }

    pub async fn group_register(
        &mut self,
        req: proto::GroupRequest,
    ) -> Result<proto::GroupResponse> {
        match self {
            Backend::Local(l) => service::group_register(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, group_register, req),
        }
    }

    pub async fn group_join(
        &mut self,
        req: proto::GroupRequest,
    ) -> Result<proto::GroupJoinResponse> {
        match self {
            Backend::Local(l) => service::group_join(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, group_join, req),
        }
    }

    pub async fn group_leave(&mut self, req: proto::GroupRequest) -> Result<proto::GroupResponse> {
        match self {
            Backend::Local(l) => service::group_leave(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, group_leave, req),
        }
    }

    pub async fn group_remove(
        &mut self,
        req: proto::GroupRemoveRequest,
    ) -> Result<proto::GroupRemoveResponse> {
        match self {
            Backend::Local(l) => service::group_remove(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, group_remove, req),
        }
    }

    pub async fn group_list(&mut self) -> Result<proto::GroupListResponse> {
        match self {
            Backend::Local(l) => service::group_list(&mut l.conn, &l.caller),
            Backend::Remote(r) => rpc!(r, group_list, proto::Empty {}),
        }
    }

    pub async fn send(&mut self, req: proto::SendRequest) -> Result<proto::SendResponse> {
        match self {
            Backend::Local(l) => service::send(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, send, req),
        }
    }

    pub async fn read(&mut self, req: proto::ReadRequest) -> Result<proto::ReadResponse> {
        match self {
            Backend::Local(l) => service::read(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, read, req),
        }
    }

    pub async fn list_agents(
        &mut self,
        req: proto::ListAgentsRequest,
    ) -> Result<proto::ListAgentsResponse> {
        match self {
            Backend::Local(l) => service::list_agents(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, list_agents, req),
        }
    }

    pub async fn bind_session(
        &mut self,
        req: proto::BindSessionRequest,
    ) -> Result<proto::BindSessionResponse> {
        match self {
            Backend::Local(l) => service::bind_session(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, bind_session, req),
        }
    }

    pub async fn session_agent(
        &mut self,
        req: proto::SessionAgentRequest,
    ) -> Result<proto::SessionAgentResponse> {
        match self {
            Backend::Local(l) => service::session_agent(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, session_agent, req),
        }
    }

    /// Watch the most recent `backlog` messages (humans only): the first
    /// snapshot comes at once, then one after every change.
    pub async fn watch_feed(&mut self, backlog: u32) -> Result<FeedWatch<'_>> {
        let req = proto::FeedRequest { backlog };
        Ok(match self {
            Backend::Local(local) => FeedWatch::Local {
                local,
                req,
                seen: None,
                next_poll: tokio::time::Instant::now(),
            },
            Backend::Remote(r) => FeedWatch::Remote(rpc!(r, watch_feed, req)?),
        })
    }

    /// Block until the caller has unread mail, then take it as `read` would.
    /// One waiter per inbox; a second fails immediately.
    pub async fn wait_for_mail(&mut self, limit: Option<u32>) -> Result<Vec<proto::Message>> {
        match self {
            Backend::Local(l) => l.wait_for_mail(limit).await,
            Backend::Remote(r) => {
                let mut stream = rpc!(r, wait_for_mail, proto::WaitRequest { limit })?;
                let mut msgs = Vec::new();
                while let Some(m) = stream.message().await.map_err(status_error)? {
                    msgs.push(m);
                }
                Ok(msgs)
            }
        }
    }
}

pub enum FeedWatch<'a> {
    /// Polls `data_version`, which changes only when another connection
    /// commits, and re-fetches after a change.
    Local {
        local: &'a mut Local,
        req: proto::FeedRequest,
        seen: Option<i64>,
        /// Kept across calls, so a cancelled `next` does not poll early.
        next_poll: tokio::time::Instant,
    },
    /// The server pushes a snapshot after every change.
    Remote(tonic::Streaming<proto::FeedSnapshot>),
}

impl FeedWatch<'_> {
    /// The next snapshot. Cancel-safe: dropping the future loses nothing.
    pub async fn next(&mut self) -> Result<Vec<proto::Message>> {
        match self {
            FeedWatch::Local {
                local,
                req,
                seen,
                next_poll,
            } => loop {
                tokio::time::sleep_until(*next_poll).await;
                *next_poll = tokio::time::Instant::now() + FEED_POLL_INTERVAL;
                let version = db::data_version(&local.conn)?;
                if *seen != Some(version) {
                    let snapshot = service::feed(&mut local.conn, &local.caller, *req)?;
                    *seen = Some(version);
                    return Ok(snapshot.messages);
                }
            },
            FeedWatch::Remote(stream) => match stream.message().await.map_err(status_error)? {
                Some(snapshot) => Ok(snapshot.messages),
                None => bail!("the server closed the feed"),
            },
        }
    }
}

impl Local {
    async fn wait_for_mail(&mut self, limit: Option<u32>) -> Result<Vec<proto::Message>> {
        let me = service::whoami(&mut self.conn, &self.caller)?;
        let me = db::Principal {
            kind: match me.kind() {
                proto::Kind::Agent => db::Kind::Agent,
                _ => db::Kind::Human,
            },
            id: me.id,
            name: me.name,
        };
        let _lock = lock_waiter(&me, &self.dir)?;
        loop {
            let req = proto::ReadRequest {
                limit,
                ..Default::default()
            };
            let msgs = service::read(&mut self.conn, &self.caller, req)?.messages;
            if !msgs.is_empty() {
                return Ok(msgs);
            }
            // Another reader of the same inbox may win the race; keep waiting until we get mail.
            self.wait_for_unread(&me).await?;
        }
    }

    /// Block until the caller has unread mail. Never holds a transaction while idle:
    /// `PRAGMA data_version` changes only when another connection commits, so we
    /// poll it once a second and run the unread query only after a change.
    async fn wait_for_unread(&mut self, me: &db::Principal) -> Result<()> {
        tracing::info!(reader = me.name, "waiting for mail");
        let mut seen_version = None;
        let mut last_touch = Instant::now();
        loop {
            let version = db::data_version(&self.conn)?;
            if seen_version != Some(version) {
                seen_version = Some(version);
                if me.kind == db::Kind::Agent && !db::agent_is_live(&self.conn, me.id)? {
                    bail!("{} was retired while waiting", me.name);
                }
                if db::has_unread(&self.conn, me)? {
                    return Ok(());
                }
            }
            // A waiting agent is alive; keep it clear of stale-agent retirement.
            if last_touch.elapsed() >= WAIT_TOUCH_INTERVAL {
                service::whoami(&mut self.conn, &self.caller)?;
                last_touch = Instant::now();
            }
            tokio::time::sleep(WAIT_POLL_INTERVAL).await;
        }
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
