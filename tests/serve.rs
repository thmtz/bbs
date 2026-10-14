//! Drive `bbs serve` over gRPC with the generated client.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tonic::Request;
use tonic::transport::Channel;
use tonic_health::pb::{
    HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
};

mod proto {
    tonic::include_proto!("bbs.v1");
}

use proto::bbs_client::BbsClient;

struct Server {
    child: Child,
    home: PathBuf,
    addr: String,
}

impl Server {
    fn start(test: &str) -> Server {
        let home = std::env::temp_dir().join(format!("bbs-serve-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bbs"))
            .args(["serve", "--listen", "127.0.0.1:0"])
            .env("HOME", &home)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .rsplit(' ')
            .next()
            .expect("listening line")
            .to_string();
        Server { child, home, addr }
    }

    async fn client(&self) -> BbsClient<Channel> {
        BbsClient::connect(format!("http://{}", self.addr))
            .await
            .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn as_agent<T>(token: &str, msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("bbs-token", token.parse().unwrap());
    req
}

fn as_human<T>(name: &str, msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("bbs-human", name.parse().unwrap());
    req
}

async fn register(client: &mut BbsClient<Channel>) -> proto::RegisterResponse {
    let req = proto::RegisterRequest {
        note: Some("testing".into()),
        pwd: "/tmp".into(),
        hostname: "test".into(),
    };
    client.register(req).await.unwrap().into_inner()
}

#[tokio::test]
async fn serve_round_trip_and_wait_for_mail() {
    let server = Server::start("round-trip");
    let mut client = server.client().await;

    let created = client
        .ensure_human(proto::EnsureHumanRequest {
            name: "will".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .created;
    assert!(created);
    let group = proto::GroupRequest {
        group: "@acme".into(),
    };
    client
        .group_register(as_human("will", group.clone()))
        .await
        .unwrap();

    let a = register(&mut client).await;
    let b = register(&mut client).await;
    for agent in [&a, &b] {
        let joined = client
            .group_join(as_agent(&agent.token, group.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((joined.group.as_str(), joined.joined), ("acme", true));
    }
    let me = client
        .whoami(as_agent(&b.token, proto::Empty {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.name, b.name);

    // Errors carry the service's message text.
    let err = client
        .send(as_agent(
            &a.token,
            proto::SendRequest {
                to: "will".into(),
                body: "hi".into(),
            },
        ))
        .await
        .unwrap_err();
    assert!(
        err.message().contains("agents cannot message humans"),
        "{err}"
    );

    // B waits; a second waiter on the same inbox is refused.
    let mut waiter = client
        .wait_for_mail(as_agent(&b.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap()
        .into_inner();
    let err = client
        .wait_for_mail(as_agent(&b.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap_err();
    assert!(err.message().contains("already running"), "{err}");
    let early = tokio::time::timeout(Duration::from_millis(300), waiter.message()).await;
    assert!(early.is_err(), "waiter returned before any mail was sent");

    let sent = client
        .send(as_agent(
            &a.token,
            proto::SendRequest {
                to: b.name.clone(),
                body: "wake up".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(sent.to, b.name);

    let got = tokio::time::timeout(Duration::from_secs(5), waiter.message())
        .await
        .expect("waiter woke")
        .unwrap()
        .expect("one message");
    assert_eq!(got.body, "wake up");
    assert_eq!(got.from_name, a.name);
    assert_eq!(got.to_name.as_deref(), Some(b.name.as_str()));
    let end = tokio::time::timeout(Duration::from_secs(5), waiter.message())
        .await
        .expect("stream ends")
        .unwrap();
    assert!(end.is_none(), "stream should end after delivering");

    // The mail was taken; the slot is free again.
    let unread = client
        .read(as_agent(&b.token, proto::ReadRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert!(unread.messages.is_empty());
    client
        .wait_for_mail(as_agent(&b.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap();
}

#[tokio::test]
async fn waiter_slot_is_released_on_disconnect_and_retirement_ends_wait() {
    let server = Server::start("slot");
    let mut client = server.client().await;
    let a = register(&mut client).await;

    let waiter = client
        .wait_for_mail(as_agent(&a.token, proto::WaitRequest { limit: None }))
        .await
        .unwrap()
        .into_inner();
    drop(waiter);
    // The server notices the dropped stream and frees the slot.
    let mut waiter = None;
    for _ in 0..50 {
        match client
            .wait_for_mail(as_agent(&a.token, proto::WaitRequest { limit: None }))
            .await
        {
            Ok(w) => {
                waiter = Some(w.into_inner());
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let mut waiter = waiter.expect("slot released after disconnect");

    client
        .bye(as_agent(&a.token, proto::Empty {}))
        .await
        .unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), waiter.message())
        .await
        .expect("waiter ended")
        .unwrap_err();
    assert!(err.message().contains("retired while waiting"), "{err}");
}

#[tokio::test]
async fn watch_feed_snapshots_on_change_for_humans_only() {
    let server = Server::start("feed");
    let mut client = server.client().await;
    client
        .ensure_human(proto::EnsureHumanRequest {
            name: "will".into(),
        })
        .await
        .unwrap();
    let a = register(&mut client).await;

    let err = client
        .watch_feed(as_agent(&a.token, proto::FeedRequest { backlog: 10 }))
        .await
        .unwrap_err();
    assert!(err.message().contains("humans only"), "{err}");

    let mut feed = client
        .watch_feed(as_human("will", proto::FeedRequest { backlog: 10 }))
        .await
        .unwrap()
        .into_inner();
    let first = feed.message().await.unwrap().unwrap();
    assert!(first.messages.is_empty());

    client
        .send(as_human(
            "will",
            proto::SendRequest {
                to: a.name.clone(),
                body: "hello".into(),
            },
        ))
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), feed.message())
        .await
        .expect("feed updated")
        .unwrap()
        .unwrap();
    assert_eq!(next.messages.len(), 1);
    assert_eq!(next.messages[0].to_name.as_deref(), Some(a.name.as_str()));
    assert_eq!(next.messages[0].read_at, None);
}

#[tokio::test]
async fn health_reports_initialized_services_without_a_human() {
    let server = Server::start("health");
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", server.addr))
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    for service in ["", "bbs.v1.Bbs"] {
        let result = health
            .check(HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(result.status(), ServingStatus::Serving);
    }
    let error = health
        .check(HealthCheckRequest {
            service: "missing".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
}

#[cfg(unix)]
fn signal(server: &Server, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &server.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
async fn expect_clean_exit(server: &mut Server, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                assert!(status.success(), "server exited with {status}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server finished shutdown within its grace period");
}

#[cfg(unix)]
#[tokio::test]
async fn termination_closes_mail_and_feed_streams() {
    for name in ["-TERM", "-INT"] {
        let mut server = Server::start(name);
        let mut client = server.client().await;
        client
            .ensure_human(proto::EnsureHumanRequest {
                name: "will".into(),
            })
            .await
            .unwrap();
        let agent = register(&mut client).await;
        let mut mail = client
            .wait_for_mail(as_agent(&agent.token, proto::WaitRequest { limit: None }))
            .await
            .unwrap()
            .into_inner();
        let mut feed = client
            .watch_feed(as_human("will", proto::FeedRequest { backlog: 10 }))
            .await
            .unwrap()
            .into_inner();
        feed.message().await.unwrap().unwrap();
        signal(&server, name);
        expect_clean_exit(&mut server, Duration::from_secs(3)).await;
        assert_eq!(
            mail.message().await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        assert_eq!(
            feed.message().await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        let db = rusqlite::Connection::open(server.home.join(".local/state/bbs/bbs.db")).unwrap();
        let integrity: String = db
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn health_watchers_cannot_prevent_shutdown() {
    let mut server = Server::start("health-watch-shutdown");
    let mut health = HealthClient::new(
        Channel::from_shared(format!("http://{}", server.addr))
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    let mut watch = health
        .watch(HealthCheckRequest {
            service: "bbs.v1.Bbs".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        watch.message().await.unwrap().unwrap().status(),
        ServingStatus::Serving
    );
    signal(&server, "-TERM");
    let last = tokio::time::timeout(Duration::from_secs(2), watch.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(last.status(), ServingStatus::NotServing);
    // Keep the health stream alive: the server must enforce its drain deadline.
    expect_clean_exit(&mut server, Duration::from_secs(10)).await;
    drop(watch);
}
