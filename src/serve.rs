//! `bbs serve`: a gRPC server over the service layer. It owns the SQLite file;
//! nothing else may open that file while it runs, because the waiters' notify
//! only sees commits made through this process.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_stream::Stream;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use crate::db;
use crate::proto::bbs_server::{Bbs, BbsServer};
use crate::proto::{self, Kind};
use crate::service::{self, Caller};

/// Keepalive pings let a vanished client free its waiter slot within ~30s.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// A waiting agent is alive; keep it clear of stale-agent retirement.
const WAIT_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub fn run(listen: SocketAddr, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut conn = db::open(&dir.join("bbs.db"))?;
    db::migrate(&mut conn)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(conn, listen))
}

async fn serve(conn: Connection, listen: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let addr = listener.local_addr()?;
    // Tests read this line to learn the port when listening on port 0.
    println!("bbs serve listening on {addr}");
    tracing::info!(%addr, "serving");
    let state = Arc::new(State {
        conn: Mutex::new(conn),
        notify: watch::Sender::new(0),
        waiters: Mutex::new(HashSet::new()),
    });
    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .add_service(BbsServer::new(Server(state)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}

struct State {
    conn: Mutex<Connection>,
    /// Bumped after every committed change; waiters and feeds re-check on it.
    notify: watch::Sender<u64>,
    /// Inboxes with a `WaitForMail` stream open, as (kind, id).
    waiters: Mutex<HashSet<(i32, i64)>>,
}

impl State {
    /// Run `f` on the connection off the async runtime.
    async fn call<T, F>(self: &Arc<Self>, f: F) -> Result<T, Status>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let state = self.clone();
        let out = tokio::task::spawn_blocking(move || {
            let mut conn = state.conn.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut conn)
        })
        .await
        .map_err(|err| {
            tracing::error!("service call failed: {err}");
            Status::internal("internal error")
        })?;
        out.map_err(|err| Status::failed_precondition(format!("{err:#}")))
    }

    fn changed(&self) {
        self.notify.send_modify(|v| *v += 1);
    }

    /// Wait for unread mail, then stream it as `Read` would take it.
    async fn wait(
        self: &Arc<Self>,
        caller: Caller,
        me: proto::Principal,
        limit: Option<u32>,
        tx: &mpsc::Sender<Result<proto::Message, Status>>,
    ) -> Result<(), Status> {
        let caller = Arc::new(caller);
        let mut changes = self.notify.subscribe();
        let mut touch =
            tokio::time::interval_at(Instant::now() + WAIT_TOUCH_INTERVAL, WAIT_TOUCH_INTERVAL);
        loop {
            // Mark seen before checking, so a commit after the check still wakes us.
            changes.borrow_and_update();
            if me.kind() == Kind::Agent {
                let id = me.id;
                if !self.call(move |c| db::agent_is_live(c, id)).await? {
                    let msg = format!("{} was retired while waiting", me.name);
                    return Err(Status::failed_precondition(msg));
                }
            }
            let c = caller.clone();
            let req = proto::ReadRequest {
                limit,
                ..Default::default()
            };
            let msgs = self.call(move |conn| service::read(conn, &c, req)).await?;
            if !msgs.messages.is_empty() {
                self.changed();
                for m in msgs.messages {
                    if tx.send(Ok(m)).await.is_err() {
                        break;
                    }
                }
                return Ok(());
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = touch.tick() => {
                    let c = caller.clone();
                    self.call(move |conn| service::whoami(conn, &c)).await?;
                }
                _ = tx.closed() => return Ok(()),
            }
        }
    }
}

/// A claimed `WaitForMail` inbox, released when the stream ends or the client goes away.
struct Slot {
    state: Arc<State>,
    key: (i32, i64),
}

impl Slot {
    fn claim(state: &Arc<State>, me: &proto::Principal) -> Result<Slot, Status> {
        let key = (me.kind, me.id);
        let mut waiters = state.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        if !waiters.insert(key) {
            let msg = format!(
                "another `bbs read --wait` is already running for {}",
                me.name
            );
            return Err(Status::failed_precondition(msg));
        }
        Ok(Slot {
            state: state.clone(),
            key,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut waiters = self
            .state
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        waiters.remove(&self.key);
    }
}

struct Server(Arc<State>);

fn caller<T>(req: &Request<T>) -> Caller {
    let get = |key| {
        req.metadata()
            .get(key)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    Caller {
        token: get("bbs-token"),
        human: get("bbs-human"),
    }
}

/// Whether a successful call changed state that waiters or feeds can observe.
#[derive(Clone, Copy, PartialEq)]
enum Effect {
    Query,
    Write,
}

impl Server {
    async fn unary<Req, T, F>(
        &self,
        req: Request<Req>,
        effect: Effect,
        f: F,
    ) -> Result<Response<T>, Status>
    where
        Req: Send + 'static,
        T: Send + 'static,
        F: FnOnce(&mut Connection, &Caller, Req) -> Result<T> + Send + 'static,
    {
        let caller = caller(&req);
        let req = req.into_inner();
        let out = self.0.call(move |conn| f(conn, &caller, req)).await?;
        if effect == Effect::Write {
            self.0.changed();
        }
        Ok(Response::new(out))
    }
}

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl Bbs for Server {
    async fn ensure_human(
        &self,
        req: Request<proto::EnsureHumanRequest>,
    ) -> Result<Response<proto::EnsureHumanResponse>, Status> {
        self.unary(req, Effect::Write, |c, _, r| service::ensure_human(c, r))
            .await
    }

    async fn add_human(
        &self,
        req: Request<proto::AddHumanRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary(req, Effect::Write, service::add_human).await
    }

    async fn register(
        &self,
        req: Request<proto::RegisterRequest>,
    ) -> Result<Response<proto::RegisterResponse>, Status> {
        self.unary(req, Effect::Write, service::register).await
    }

    async fn whoami(
        &self,
        req: Request<proto::Empty>,
    ) -> Result<Response<proto::Principal>, Status> {
        self.unary(req, Effect::Query, |c, caller, _| {
            service::whoami(c, caller)
        })
        .await
    }

    async fn set_status(
        &self,
        req: Request<proto::SetStatusRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.unary(req, Effect::Write, service::set_status).await
    }

    async fn bye(
        &self,
        req: Request<proto::Empty>,
    ) -> Result<Response<proto::ByeResponse>, Status> {
        self.unary(req, Effect::Write, |c, caller, _| service::bye(c, caller))
            .await
    }

    async fn group_register(
        &self,
        req: Request<proto::GroupRequest>,
    ) -> Result<Response<proto::GroupResponse>, Status> {
        self.unary(req, Effect::Write, service::group_register)
            .await
    }

    async fn group_join(
        &self,
        req: Request<proto::GroupRequest>,
    ) -> Result<Response<proto::GroupJoinResponse>, Status> {
        self.unary(req, Effect::Write, service::group_join).await
    }

    async fn group_leave(
        &self,
        req: Request<proto::GroupRequest>,
    ) -> Result<Response<proto::GroupResponse>, Status> {
        self.unary(req, Effect::Write, service::group_leave).await
    }

    async fn group_remove(
        &self,
        req: Request<proto::GroupRemoveRequest>,
    ) -> Result<Response<proto::GroupRemoveResponse>, Status> {
        self.unary(req, Effect::Write, service::group_remove).await
    }

    async fn group_list(
        &self,
        req: Request<proto::Empty>,
    ) -> Result<Response<proto::GroupListResponse>, Status> {
        self.unary(req, Effect::Query, |c, caller, _| {
            service::group_list(c, caller)
        })
        .await
    }

    async fn send(
        &self,
        req: Request<proto::SendRequest>,
    ) -> Result<Response<proto::SendResponse>, Status> {
        self.unary(req, Effect::Write, service::send).await
    }

    async fn read(
        &self,
        req: Request<proto::ReadRequest>,
    ) -> Result<Response<proto::ReadResponse>, Status> {
        let resp = self.unary(req, Effect::Query, service::read).await?;
        // Taking mail sets read_at, which the feed shows.
        if !resp.get_ref().messages.is_empty() {
            self.0.changed();
        }
        Ok(resp)
    }

    type WaitForMailStream = BoxStream<proto::Message>;

    async fn wait_for_mail(
        &self,
        req: Request<proto::WaitRequest>,
    ) -> Result<Response<Self::WaitForMailStream>, Status> {
        let caller = caller(&req);
        let limit = req.into_inner().limit;
        let c = caller.clone();
        let me = self.0.call(move |conn| service::whoami(conn, &c)).await?;
        // Claim before returning, so a second waiter fails immediately.
        let slot = Slot::claim(&self.0, &me)?;
        let (tx, rx) = mpsc::channel(16);
        let state = self.0.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(status) = state.wait(caller, me, limit, &tx).await {
                let _ = tx.send(Err(status)).await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn list_agents(
        &self,
        req: Request<proto::ListAgentsRequest>,
    ) -> Result<Response<proto::ListAgentsResponse>, Status> {
        self.unary(req, Effect::Query, service::list_agents).await
    }

    type WatchFeedStream = BoxStream<proto::FeedSnapshot>;

    async fn watch_feed(
        &self,
        req: Request<proto::FeedRequest>,
    ) -> Result<Response<Self::WatchFeedStream>, Status> {
        let caller = Arc::new(caller(&req));
        let req = req.into_inner();
        let state = self.0.clone();
        let mut changes = state.notify.subscribe();
        // The first snapshot also checks the caller is a human, before the stream opens.
        let c = caller.clone();
        let first = state.call(move |conn| service::feed(conn, &c, req)).await?;
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut snapshot = Ok(first);
            loop {
                let failed = snapshot.is_err();
                if tx.send(snapshot).await.is_err() || failed {
                    return;
                }
                tokio::select! {
                    _ = changes.changed() => {}
                    _ = tx.closed() => return,
                }
                changes.borrow_and_update();
                let c = caller.clone();
                snapshot = state.call(move |conn| service::feed(conn, &c, req)).await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn bind_session(
        &self,
        req: Request<proto::BindSessionRequest>,
    ) -> Result<Response<proto::BindSessionResponse>, Status> {
        self.unary(req, Effect::Write, |c, _, r| service::bind_session(c, r))
            .await
    }

    async fn session_agent(
        &self,
        req: Request<proto::SessionAgentRequest>,
    ) -> Result<Response<proto::SessionAgentResponse>, Status> {
        self.unary(req, Effect::Query, |c, _, r| service::session_agent(c, r))
            .await
    }
}
