//! Where commands run: in-process against the local SQLite file, or over gRPC
//! against a `bbs serve` process. Both take and return the protobuf types, so
//! the CLI formats the same responses either way.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};

use crate::config::Config;
use crate::db;
use crate::proto::{self, bbs_client::BbsClient};
use crate::protocol;
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
    fn request<T>(&self, msg: T, capabilities: &[&str]) -> Result<tonic::Request<T>> {
        let mut req = tonic::Request::new(msg);
        protocol::client_metadata(req.metadata_mut(), capabilities);
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
    let message = if status.code() == tonic::Code::Unimplemented {
        "BBS v2 service is unavailable; coordinate a client/server upgrade".to_string()
    } else {
        status.message().to_string()
    };
    anyhow::Error::new(status).context(message)
}

/// One unary RPC: `rpc!(remote, method, request)`.
macro_rules! rpc {
    ($remote:expr, $method:ident, $msg:expr) => {
        rpc!($remote, $method, $msg, &[])
    };
    ($remote:expr, $method:ident, $msg:expr, $caps:expr) => {{
        let req = $remote.request($msg, $caps)?;
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
            Backend::Remote(r) => {
                let capabilities: &[&str] = if req.name.is_some() {
                    &["named-agents.v1"]
                } else {
                    &[]
                };
                rpc!(r, register, req, capabilities)
            }
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
            Backend::Remote(r) => {
                let mut req = r.request(proto::Empty {}, &[])?;
                req.set_timeout(CONNECT_TIMEOUT);
                let response = tokio::time::timeout(CONNECT_TIMEOUT, r.client.server_info(req))
                    .await
                    .map_err(|_| {
                        status_error(tonic::Status::deadline_exceeded("BBS ServerInfo timed out"))
                    })?
                    .map_err(status_error)?;
                let info = response.into_inner();
                protocol::validate_info(&info)?;
                Ok(info)
            }
            Backend::Local(_) => Ok(protocol::Policy::default().info(false)),
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

    pub async fn group_delete(&mut self, req: proto::GroupRequest) -> Result<proto::GroupResponse> {
        match self {
            Backend::Local(l) => service::group_delete(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, group_delete, req, &["group-archive.v1"]),
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
            Backend::Remote(r) => rpc!(r, read, req, &["owner-attribution.v1"]),
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
            Backend::Remote(r) => rpc!(r, bind_session, req, &["human-token-sessions.v1"]),
        }
    }

    pub async fn session_agent(
        &mut self,
        req: proto::SessionAgentRequest,
    ) -> Result<proto::SessionAgentResponse> {
        match self {
            Backend::Local(l) => service::session_agent(&mut l.conn, &l.caller, req),
            Backend::Remote(r) => rpc!(r, session_agent, req, &["human-token-sessions.v1"]),
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
            Backend::Remote(r) => {
                FeedWatch::Remote(rpc!(r, watch_feed, req, &["owner-attribution.v1"])?)
            }
        })
    }

    /// Block until the caller has unread mail, then take it as `read` would.
    /// One waiter per inbox; a second fails immediately.
    pub async fn wait_for_mail(&mut self, limit: Option<u32>) -> Result<Vec<proto::Message>> {
        match self {
            Backend::Local(l) => l.wait_for_mail(limit).await,
            Backend::Remote(r) => {
                let mut stream = rpc!(
                    r,
                    wait_for_mail,
                    proto::WaitRequest { limit },
                    &["owner-attribution.v1"]
                )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};
    use tonic::body::Body;
    use tonic::codegen::{BoxFuture, Bytes, Service, http};

    #[test]
    fn status_conversion_retains_code_and_metadata() {
        for code in [
            tonic::Code::Unimplemented,
            tonic::Code::Unauthenticated,
            tonic::Code::Unavailable,
            tonic::Code::FailedPrecondition,
        ] {
            let mut original = tonic::Status::new(code, "test failure");
            original
                .metadata_mut()
                .insert("bbs-protocol-epoch", "2".parse().unwrap());
            let error = status_error(original);
            let status = error.downcast_ref::<tonic::Status>().unwrap();
            assert_eq!(status.code(), code);
            assert_eq!(status.metadata().get("bbs-protocol-epoch").unwrap(), "2");
            if code == tonic::Code::Unimplemented {
                assert!(
                    error
                        .to_string()
                        .contains("coordinate a client/server upgrade")
                );
            }
        }
    }

    struct PendingBody;
    impl http_body::Body for PendingBody {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
            Poll::Pending
        }
    }

    #[derive(Clone, Default)]
    struct RecordingService(std::sync::Arc<std::sync::Mutex<Vec<(String, http::HeaderMap)>>>);
    impl tonic::server::NamedService for RecordingService {
        const NAME: &'static str = "bbs.v2.Bbs";
    }
    impl Service<http::Request<Body>> for RecordingService {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            self.0
                .lock()
                .unwrap()
                .push((req.uri().path().to_owned(), req.headers().clone()));
            Box::pin(async { Ok(tonic::Status::failed_precondition("recorded").into_http()) })
        }
    }

    #[tokio::test]
    async fn backend_sends_required_capabilities_without_discovery_or_retries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let recording = RecordingService::default();
        let observations = recording.0.clone();
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(recording)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        let remote = Remote::connect(&address.to_string(), Caller::default())
            .await
            .unwrap();
        let mut backend = Backend::Remote(remote);
        assert!(
            backend
                .ensure_human(proto::EnsureHumanRequest {
                    name: "test".into()
                })
                .await
                .is_err()
        );
        assert!(
            backend
                .register(proto::RegisterRequest {
                    name: Some("Named".into()),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            backend
                .register(proto::RegisterRequest::default())
                .await
                .is_err()
        );
        assert!(backend.read(proto::ReadRequest::default()).await.is_err());
        assert!(backend.wait_for_mail(None).await.is_err());
        assert!(backend.watch_feed(10).await.is_err());
        assert!(
            backend
                .bind_session(proto::BindSessionRequest::default())
                .await
                .is_err()
        );
        assert!(
            backend
                .session_agent(proto::SessionAgentRequest::default())
                .await
                .is_err()
        );
        assert!(
            backend
                .group_delete(proto::GroupRequest::default())
                .await
                .is_err()
        );
        server.abort();
        let expected = [
            ("EnsureHuman", None),
            ("Register", Some("named-agents.v1")),
            ("Register", None),
            ("Read", Some("owner-attribution.v1")),
            ("WaitForMail", Some("owner-attribution.v1")),
            ("WatchFeed", Some("owner-attribution.v1")),
            ("BindSession", Some("human-token-sessions.v1")),
            ("SessionAgent", Some("human-token-sessions.v1")),
            ("GroupDelete", Some("group-archive.v1")),
        ];
        let observed = observations.lock().unwrap();
        assert_eq!(
            observed.len(),
            expected.len(),
            "no preflight or automatic retry"
        );
        for ((path, headers), (method, capabilities)) in observed.iter().zip(expected) {
            assert_eq!(path, &format!("/bbs.v2.Bbs/{method}"));
            assert_eq!(headers["bbs-protocol-epoch"], "2");
            assert_eq!(headers["bbs-client-version"], crate::release::VERSION);
            assert_eq!(
                headers
                    .get("bbs-required-capabilities")
                    .map(|v| v.to_str().unwrap()),
                capabilities
            );
        }
    }

    #[derive(Clone)]
    struct StalledInfo {
        send_headers: bool,
    }
    impl tonic::server::NamedService for StalledInfo {
        const NAME: &'static str = "bbs.v2.Bbs";
    }
    impl Service<http::Request<Body>> for StalledInfo {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            assert_eq!(req.uri().path(), "/bbs.v2.Bbs/ServerInfo");
            let send_headers = self.send_headers;
            Box::pin(async move {
                if !send_headers {
                    return std::future::pending().await;
                }
                Ok(http::Response::builder()
                    .header("content-type", "application/grpc")
                    .body(Body::new(PendingBody))
                    .unwrap())
            })
        }
    }

    #[tokio::test]
    async fn discovery_deadline_includes_headers_and_body() {
        for send_headers in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(StalledInfo { send_headers })
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
            );
            let remote = Remote::connect(&address.to_string(), Caller::default())
                .await
                .unwrap();
            let mut backend = Backend::Remote(remote);
            let result = tokio::time::timeout(Duration::from_secs(4), backend.server_info()).await;
            server.abort();
            let error = result
                .expect("discovery deadline must include a stalled response body")
                .unwrap_err();
            let code = error.downcast_ref::<tonic::Status>().unwrap().code();
            // Tonic may expire the transmitted grpc-timeout first while waiting for headers.
            assert!(
                matches!(code, tonic::Code::DeadlineExceeded | tonic::Code::Cancelled),
                "{error:#}"
            );
        }
    }
}
