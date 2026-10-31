//! End-to-end tests of the NNTP client against the testkit mock server and small
//! scripted servers on real loopback TCP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use newskite_nntp::testkit::{MockConfig, MockServer};
use newskite_nntp::{test_server, NntpConnection, NntpError};
use newskite_types::ServerConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(5);
const USER: &str = "mockuser";
const PASS: &str = "hunter2-secret";

fn server_cfg(addr: SocketAddr, user: &str, pass: &str) -> ServerConfig {
    ServerConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        tls: false,
        username: user.into(),
        password: pass.into(),
        ..Default::default()
    }
}

fn articles(items: &[(&str, &[u8])]) -> HashMap<String, Vec<u8>> {
    items.iter().map(|(k, v)| ((*k).to_owned(), v.to_vec())).collect()
}

async fn auth_mock(items: &[(&str, &[u8])]) -> MockServer {
    MockServer::start(MockConfig {
        username: Some(USER.into()),
        password: Some(PASS.into()),
        articles: articles(items),
        ..Default::default()
    })
    .await
}

/// A one-connection server that sends `greeting`, then for each step reads one line and
/// answers with the given reply. Returns every line the client sent (until EOF).
async fn scripted(greeting: &'static str, steps: Vec<&'static str>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (r, mut w) = socket.into_split();
        let mut r = BufReader::new(r);
        let _ = w.write_all(greeting.as_bytes()).await;
        let mut received = Vec::new();
        let mut steps = steps.into_iter();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            received.push(line.trim_end().to_owned());
            match steps.next() {
                Some(reply) => {
                    let _ = w.write_all(reply.as_bytes()).await;
                }
                None => {
                    let _ = w.write_all(b"205 bye\r\n").await;
                }
            }
        }
        received
    });
    (addr, handle)
}

#[tokio::test]
async fn auth_success_and_body() {
    let mock = auth_mock(&[("seg1@example", b"hello\r\nworld\r\n")]).await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, USER, PASS), TIMEOUT).await.unwrap();
    assert!(conn.greeting().starts_with("200 "), "{}", conn.greeting());
    assert!(!conn.is_tls());
    assert_eq!(conn.body("seg1@example").await.unwrap(), b"hello\r\nworld\r\n");
    assert_eq!(conn.body("<seg1@example>").await.unwrap(), b"hello\r\nworld\r\n");
    assert_eq!(mock.body_requests(), 2);
    assert_eq!(mock.connections_accepted(), 1);
    conn.quit().await;
}

#[tokio::test]
async fn auth_failure_reports_server_text() {
    let mock = auth_mock(&[]).await;
    let err = NntpConnection::connect(&server_cfg(mock.addr, USER, "wrong"), TIMEOUT).await.unwrap_err();
    match err {
        NntpError::AuthFailed(text) => {
            assert!(text.starts_with("481"), "{text}");
            assert!(!text.contains("wrong"));
        }
        other => panic!("expected AuthFailed, got {other:?}"),
    }
    let err = NntpConnection::connect(&server_cfg(mock.addr, "someone-else", PASS), TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::AuthFailed(_)), "{err:?}");
    assert!(!NntpError::AuthFailed(String::new()).is_transient());
}

#[tokio::test]
async fn missing_credentials_yield_auth_required() {
    let mock = auth_mock(&[("a@b", b"x\r\n")]).await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    assert!(matches!(conn.body("a@b").await, Err(NntpError::AuthRequired)));
    assert!(matches!(conn.stat("a@b").await, Err(NntpError::AuthRequired)));
    assert!(matches!(conn.group("alt.binaries.test").await, Err(NntpError::AuthRequired)));
    // DATE is allowed without auth.
    assert_eq!(conn.date().await.unwrap().code, 111);
}

#[tokio::test]
async fn body_round_trip_unstuffs_dots() {
    let payload: &[u8] =
        b".starts with dot\r\n..two dots\r\n.\r\nmiddle\r\n...\r\n\r\nend . here\r\n.\r\n";
    let mock = MockServer::start(MockConfig { articles: articles(&[("dots@x", payload)]), ..Default::default() }).await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    assert_eq!(conn.body("dots@x").await.unwrap(), payload);
    // The connection stays in sync for further commands.
    assert_eq!(conn.body("dots@x").await.unwrap(), payload);
    assert!(conn.stat("dots@x").await.unwrap());
    conn.quit().await;
}

#[tokio::test]
async fn body_binary_bytes_and_bare_lf_payload() {
    // Every byte value except CR/LF (yEnc never puts raw CR/LF in a line).
    let line: Vec<u8> = (0u8..=255).filter(|b| *b != b'\r' && *b != b'\n').collect();
    let mut payload = line.clone();
    payload.extend_from_slice(b"\r\n");
    let mock = MockServer::start(MockConfig {
        articles: articles(&[("bin@x", &payload), ("lf@x", b"a\nb\n"), ("empty@x", b"")]),
        ..Default::default()
    })
    .await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    assert_eq!(conn.body("bin@x").await.unwrap(), payload);
    assert_eq!(conn.body("lf@x").await.unwrap(), b"a\r\nb\r\n");
    assert_eq!(conn.body("empty@x").await.unwrap(), b"");
}

#[tokio::test]
async fn server_using_bare_lf_is_tolerated() {
    let (addr, server) = scripted("200 sloppy\n", vec!["222 0 <a@b>\n..x\nplain\n.\n"]).await;
    let mut conn = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap();
    assert_eq!(conn.greeting(), "200 sloppy");
    assert_eq!(conn.body("a@b").await.unwrap(), b".x\r\nplain\r\n");
    conn.quit().await;
    let sent = server.await.unwrap();
    assert_eq!(sent, vec!["BODY <a@b>", "QUIT"]);
}

#[tokio::test]
async fn missing_article_is_article_missing() {
    let mock = MockServer::start(MockConfig::default()).await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    match conn.body("<gone@example>").await {
        Err(NntpError::ArticleMissing(id)) => assert_eq!(id, "gone@example"),
        other => panic!("expected ArticleMissing, got {other:?}"),
    }
    assert!(!conn.stat("gone@example").await.unwrap());
    // Still usable afterwards.
    assert!(!conn.is_broken());
    assert_eq!(conn.date().await.unwrap().code, 111);
}

#[tokio::test]
async fn reauthenticates_once_on_480() {
    let (addr, server) = scripted(
        "200 ready\r\n",
        vec![
            "381 more\r\n",
            "281 ok\r\n",
            "480 auth expired\r\n",
            "381 more\r\n",
            "281 ok\r\n",
            "222 0 <m@x>\r\npayload\r\n.\r\n",
        ],
    )
    .await;
    let mut conn = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap();
    assert_eq!(conn.body("m@x").await.unwrap(), b"payload\r\n");
    conn.quit().await;
    let sent = server.await.unwrap();
    let user = format!("AUTHINFO USER {USER}");
    let pass = format!("AUTHINFO PASS {PASS}");
    assert_eq!(sent, vec![&user, &pass, "BODY <m@x>", &user, &pass, "BODY <m@x>", "QUIT"]);
}

#[tokio::test]
async fn persistent_480_after_reauth_is_auth_required() {
    let (addr, _server) = scripted(
        "200 ready\r\n",
        vec!["281 ok\r\n", "480 no\r\n", "281 ok\r\n", "480 still no\r\n"],
    )
    .await;
    let mut conn = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap();
    assert!(matches!(conn.body("m@x").await, Err(NntpError::AuthRequired)));
}

#[tokio::test]
async fn accepts_281_directly_after_user() {
    let (addr, server) = scripted("201 no posting\r\n", vec!["281 welcome\r\n", "111 20261031120000\r\n"]).await;
    let mut conn = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap();
    assert_eq!(conn.date().await.unwrap().text, "20261031120000");
    conn.quit().await;
    let sent = server.await.unwrap();
    assert_eq!(sent[0], format!("AUTHINFO USER {USER}"));
    assert!(!sent.iter().any(|l| l.contains("PASS")));
}

#[tokio::test]
async fn auth_502_is_auth_failed() {
    let (addr, _server) = scripted("200 ready\r\n", vec!["502 not permitted\r\n"]).await;
    let err = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap_err();
    assert!(matches!(&err, NntpError::AuthFailed(t) if t == "502 not permitted"), "{err:?}");
}

/// Connection-limit and overload replies at AUTHINFO time must stay transient so the
/// engine backs off instead of disabling a server whose other sessions are working.
#[tokio::test]
async fn auth_connection_limit_replies_are_transient() {
    let cases: [(&'static str, &'static str); 4] = [
        ("381 password required\r\n", "502 Too many connections\r\n"),
        ("381 password required\r\n", "481 connection limit reached for this account\r\n"),
        ("381 password required\r\n", "482 Max connections exceeded\r\n"),
        ("400 service temporarily unavailable\r\n", "281 unused\r\n"),
    ];
    for (first, second) in cases {
        let (addr, _server) = scripted("200 ready\r\n", vec![first, second]).await;
        let err = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap_err();
        assert!(matches!(err, NntpError::Connect(_)), "{first:?}/{second:?}: {err:?}");
        assert!(err.is_transient());
    }
    // A plain credential rejection is still permanent.
    let (addr, _server) =
        scripted("200 ready\r\n", vec!["381 password required\r\n", "481 Authentication failed\r\n"]).await;
    let err = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap_err();
    assert!(matches!(&err, NntpError::AuthFailed(t) if t == "481 Authentication failed"), "{err:?}");
}

/// A server that echoes the credentials back must not leak them into error text.
#[tokio::test]
async fn auth_error_text_is_redacted() {
    let (addr, _server) = scripted(
        "200 ready\r\n",
        vec!["381 password required\r\n", "481 invalid credentials mockuser/hunter2-secret\r\n"],
    )
    .await;
    let err = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, NntpError::AuthFailed(_)), "{err:?}");
    assert!(!text.contains(PASS) && !text.contains(USER), "{text}");
    assert!(text.contains("481 invalid credentials ***/***"), "{text}");
}

/// BODY refusals other than 430/423/480 must not surface as transient `Protocol` errors
/// that the engine would retry until the whole job aborts.
#[tokio::test]
async fn body_refusals_are_classified() {
    let (addr, _server) = scripted(
        "200 ready\r\n",
        vec![
            "381 password required\r\n",
            "281 ok\r\n",
            "451 removed by request\r\n",
            "503 program fault\r\n",
            "502 Access denied, account expired mockuser\r\n",
        ],
    )
    .await;
    let mut conn = NntpConnection::connect(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap();
    let err = conn.body("a@b").await.unwrap_err();
    assert!(matches!(&err, NntpError::ArticleMissing(id) if id == "a@b"), "{err:?}");
    let err = conn.body("<c@d>").await.unwrap_err();
    assert!(matches!(&err, NntpError::ArticleMissing(id) if id == "c@d"), "{err:?}");
    assert!(!conn.is_broken());
    let err = conn.body("e@f").await.unwrap_err();
    assert!(
        matches!(&err, NntpError::AuthFailed(t) if t == "502 Access denied, account expired ***"),
        "{err:?}"
    );
    assert!(!err.is_transient());

    for reply in ["481 session revoked\r\n", "482 nope\r\n"] {
        let (addr, _server) = scripted("200 ready\r\n", vec![reply]).await;
        let mut conn = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap();
        let err = conn.body("a@b").await.unwrap_err();
        assert!(matches!(err, NntpError::AuthFailed(_)), "{reply:?}: {err:?}");
    }

    for reply in ["400 shutting down\r\n", "502 Too many connections\r\n"] {
        let (addr, _server) = scripted("200 ready\r\n", vec![reply]).await;
        let mut conn = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap();
        let err = conn.body("a@b").await.unwrap_err();
        assert!(matches!(err, NntpError::Connect(_)), "{reply:?}: {err:?}");
        assert!(err.is_transient());
        assert!(conn.is_broken());
    }
}

#[tokio::test]
async fn invalid_message_ids_are_rejected_without_sending() {
    let mock = MockServer::start(MockConfig::default()).await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    for bad in ["", "a b", "x\r\nQUIT", "x\0y", "<>", "<unterminated"] {
        assert!(matches!(conn.body(bad).await, Err(NntpError::InvalidMessageId(_))), "{bad:?}");
        assert!(matches!(conn.stat(bad).await, Err(NntpError::InvalidMessageId(_))), "{bad:?}");
    }
    assert_eq!(mock.body_requests(), 0);
    assert!(!conn.is_broken());
}

#[tokio::test]
async fn command_rejects_line_breaks_and_reports_unknown() {
    let mock = MockServer::start(MockConfig::default()).await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    assert!(matches!(conn.command("DATE\r\nQUIT").await, Err(NntpError::Protocol(_))));
    assert_eq!(conn.command("FROBNICATE").await.unwrap().code, 500);
    let g = conn.group("alt.binaries.test").await.unwrap();
    assert_eq!(g.code, 211);
    assert_eq!(g.text, "0 0 0 alt.binaries.test");
    let d = conn.date().await.unwrap();
    assert_eq!(d.text.len(), 14);
    assert!(d.text.bytes().all(|b| b.is_ascii_digit()));
    assert!(d.text.starts_with("20"));
}

#[tokio::test]
async fn test_server_transcript_masks_credentials() {
    let mock = auth_mock(&[]).await;
    let transcript = test_server(&server_cfg(mock.addr, USER, PASS), TIMEOUT).await.unwrap();
    assert!(!transcript.contains(PASS), "{transcript}");
    assert!(!transcript.contains(USER), "{transcript}");
    let lines: Vec<&str> = transcript.lines().collect();
    assert!(lines[0].starts_with("<- 200 "), "{transcript}");
    assert_eq!(lines[1], "-> AUTHINFO USER ***");
    assert!(lines[2].starts_with("<- 381"));
    assert_eq!(lines[3], "-> AUTHINFO PASS ***");
    assert!(lines[4].starts_with("<- 281"));
    assert_eq!(lines[5], "-> DATE");
    assert!(lines[6].starts_with("<- 111 20"), "{transcript}");
    assert_eq!(lines.len(), 7, "{transcript}");
}

#[tokio::test]
async fn test_server_redacts_credentials_echoed_by_server() {
    let (addr, _server) = scripted(
        "200 ready\r\n",
        vec!["381 hello mockuser\r\n", "281 welcome mockuser (hunter2-secret)\r\n", "111 20261031120000\r\n"],
    )
    .await;
    let transcript = test_server(&server_cfg(addr, USER, PASS), TIMEOUT).await.unwrap();
    assert!(!transcript.contains(PASS), "{transcript}");
    assert!(!transcript.contains(USER), "{transcript}");
    assert!(transcript.contains("<- 111 20261031120000"));
}

#[tokio::test]
async fn test_server_without_auth_and_failing_auth() {
    let open = MockServer::start(MockConfig::default()).await;
    let t = test_server(&server_cfg(open.addr, "", ""), TIMEOUT).await.unwrap();
    assert_eq!(t.lines().count(), 3, "{t}");
    assert!(!t.contains("AUTHINFO"));

    let locked = auth_mock(&[]).await;
    let err = test_server(&server_cfg(locked.addr, USER, "bad"), TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::AuthFailed(_)), "{err:?}");
}

#[tokio::test]
async fn connection_refused_is_transient() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let err = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::Connect(_) | NntpError::Io(_)), "{err:?}");
    assert!(err.is_transient());
}

#[tokio::test]
async fn empty_host_is_connect_error() {
    let cfg = ServerConfig { host: "  ".into(), ..Default::default() };
    let err = NntpConnection::connect(&cfg, TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::Connect(_)), "{err:?}");
}

#[tokio::test]
async fn silent_server_times_out_during_greeting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _holder = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });
    let started = std::time::Instant::now();
    let err = NntpConnection::connect(&server_cfg(addr, "", ""), Duration::from_millis(300)).await.unwrap_err();
    assert!(matches!(err, NntpError::Timeout), "{err:?}");
    assert!(err.is_transient());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn slow_body_hits_read_timeout_and_breaks_connection() {
    let mock = MockServer::start(MockConfig {
        articles: articles(&[("slow@x", b"data\r\n")]),
        delay_ms: 1_500,
        ..Default::default()
    })
    .await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    conn.set_read_timeout(Duration::from_millis(200));
    assert!(matches!(conn.body("slow@x").await, Err(NntpError::Timeout)));
    assert!(conn.is_broken());
    assert!(matches!(conn.date().await, Err(NntpError::Protocol(_))));
}

#[tokio::test]
async fn delay_is_applied_per_body() {
    let mock = MockServer::start(MockConfig {
        articles: articles(&[("d@x", b"x\r\n")]),
        delay_ms: 150,
        ..Default::default()
    })
    .await;
    let mut conn = NntpConnection::connect(&server_cfg(mock.addr, "", ""), TIMEOUT).await.unwrap();
    let started = std::time::Instant::now();
    conn.body("d@x").await.unwrap();
    conn.body("d@x").await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn fail_first_n_connections_then_succeed() {
    let mock = MockServer::start(MockConfig { fail_first_n_connections: 2, ..Default::default() }).await;
    let cfg = server_cfg(mock.addr, "", "");
    for _ in 0..2 {
        let err = NntpConnection::connect(&cfg, TIMEOUT).await.unwrap_err();
        assert!(err.is_transient(), "{err:?}");
    }
    let conn = NntpConnection::connect(&cfg, TIMEOUT).await.unwrap();
    assert_eq!(mock.connections_accepted(), 3);
    conn.quit().await;
}

#[tokio::test]
async fn unexpected_eof_mid_body_is_io_error() {
    let (addr, _server) = scripted("200 ready\r\n", vec!["222 0 <a@b>\r\npartial line\r\nno terminator"]).await;
    let mut conn = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap();
    // The scripted server keeps the socket open until the client closes, so shorten
    // the wait; either an EOF or a timeout is a transient failure here.
    conn.set_read_timeout(Duration::from_millis(300));
    let err = conn.body("a@b").await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    assert!(conn.is_broken());
}

#[tokio::test]
async fn closed_mid_body_is_io_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (r, mut w) = socket.into_split();
        let mut r = BufReader::new(r);
        w.write_all(b"200 ready\r\n").await.unwrap();
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        w.write_all(b"222 0 <a@b>\r\nhalf").await.unwrap();
        // Dropping both halves closes the socket.
    });
    let mut conn = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap();
    let err = conn.body("a@b").await.unwrap_err();
    assert!(matches!(err, NntpError::Io(_)), "{err:?}");
}

#[tokio::test]
async fn bad_greetings() {
    let (addr, _s) = scripted("502 go away\r\n", vec![]).await;
    let err = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap_err();
    assert!(matches!(&err, NntpError::Connect(t) if t.contains("502 go away")), "{err:?}");

    let (addr, _s) = scripted("HTTP/1.1 400 Bad Request\r\n", vec![]).await;
    let err = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::Protocol(_)), "{err:?}");

    // A "status line" far beyond the limit is refused, not buffered forever.
    let huge: &'static str = Box::leak("2".repeat(40_000).into_boxed_str());
    let (addr, _s) = scripted(huge, vec![]).await;
    let err = NntpConnection::connect(&server_cfg(addr, "", ""), TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn tls_against_plain_server_fails_cleanly() {
    let mock = MockServer::start(MockConfig::default()).await;
    let mut cfg = server_cfg(mock.addr, "", "");
    cfg.tls = true;
    let err = NntpConnection::connect(&cfg, TIMEOUT).await.unwrap_err();
    assert!(matches!(err, NntpError::Tls(_) | NntpError::Io(_)), "{err:?}");
    assert!(err.is_transient());
}

#[tokio::test]
async fn shutdown_stops_accepting() {
    let mock = MockServer::start(MockConfig::default()).await;
    let addr = mock.addr;
    let cfg = server_cfg(addr, "", "");
    NntpConnection::connect(&cfg, TIMEOUT).await.unwrap().quit().await;
    mock.shutdown();
    let mut refused = false;
    for _ in 0..50 {
        if NntpConnection::connect(&cfg, Duration::from_millis(200)).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "server still accepting after shutdown");
}

#[tokio::test]
async fn concurrent_connections_share_counters() {
    let mock = auth_mock(&[("c@x", b"abc\r\n")]).await;
    let cfg = server_cfg(mock.addr, USER, PASS);
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let cfg = cfg.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = NntpConnection::connect(&cfg, TIMEOUT).await.unwrap();
            for _ in 0..5 {
                assert_eq!(c.body("c@x").await.unwrap(), b"abc\r\n");
            }
            c.quit().await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(mock.connections_accepted(), 4);
    assert_eq!(mock.body_requests(), 20);
}
