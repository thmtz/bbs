//! Drive `bbs serve` over gRPC with the generated client.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tonic::Request;
use tonic::transport::Channel;

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
    register_as(client, "will", None).await
}

async fn register_as(
    client: &mut BbsClient<Channel>,
    human: &str,
    name: Option<&str>,
) -> proto::RegisterResponse {
    client
        .ensure_human(proto::EnsureHumanRequest { name: human.into() })
        .await
        .unwrap();
    let req = proto::RegisterRequest {
        name: name.map(str::to_string),
        note: Some("testing".into()),
        pwd: "/tmp".into(),
        hostname: "test".into(),
    };
    client
        .register(as_human(human, req))
        .await
        .unwrap()
        .into_inner()
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

    let a = register_as(&mut client, "will", Some("Jeffrey")).await;
    let b = register_as(&mut client, "ada", None).await;
    assert_eq!(a.name, "Jeffrey");
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
    let agents = client
        .list_agents(as_agent(&b.token, proto::ListAgentsRequest { all: false }))
        .await
        .unwrap()
        .into_inner()
        .agents;
    let sender = agents.iter().find(|agent| agent.name == a.name).unwrap();
    assert_eq!(sender.name, "Jeffrey");
    assert_eq!(sender.owner.as_deref(), Some("will"));

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
    assert_eq!(got.from_owner.as_deref(), Some("will"));
    assert_eq!(got.reader_owner.as_deref(), Some("ada"));
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
