//! Shared integration-test harness: a scriptable in-process NNTP server.
//!
//! Unlike the tiny hard-coded fake in `nntp.rs` unit tests, this server can
//! be told to behave pathologically — deliver bytes one at a time, stall,
//! drop the socket mid-body, reject auth, and serve dot-stuffed bodies — so
//! the whole NNTP client (§2 of TEST_PLAN.md) can be tested hermetically in
//! CI with no live news server.

// The harness is recompiled into each test binary, which each use a subset
// of its features; silence per-binary dead-code / assignment warnings.
#![allow(dead_code, unused_assignments)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split};
use tokio::net::{TcpListener, TcpStream};

/// An article the mock server can serve via BODY/STAT.
#[derive(Clone)]
pub struct MockArticle {
    /// Dot-stuffed (on-the-wire) body *without* the trailing `.\r\n`.
    /// Lines must already have `..` doubling applied where the client will
    /// expect it.
    pub wire_body: Vec<u8>,
    /// If true, BODY returns 430 and STAT returns 430.
    pub missing: bool,
}

/// How AUTHINFO USER/PASS is handled.
#[derive(Clone, Copy, Default)]
pub enum AuthMode {
    /// No auth required — any AUTHINFO sequence let through.
    #[default]
    Accept,
    /// Standard 381 → 281 challenge.
    Challenge,
    /// Always reject with 502 (but allow post-auth commands anyway is not
    /// realistic; test expects connect to fail).
    Reject,
}

/// Scripted pathologies to test the client's robustness.
#[derive(Clone, Default)]
pub struct MockOptions {
    /// Greeting line (bytes, `\r\n` included). Defaults to
    /// `200 turbonzb-mock ready`.
    pub greeting: Option<Vec<u8>>,
    pub auth_mode: AuthMode,
    /// Write each response byte individually (splits every line across many
    /// reads, exercising partial-line handling).
    pub bytewise: bool,
    /// After receiving a command, sleep this long before first write.
    pub delay: Option<Duration>,
    /// Write at most this many bytes before closing the socket mid-response.
    pub drop_after_bytes: Option<usize>,
    /// Number of successful BODY completions before the server closes the
    /// connection (to exercise reconnect).
    pub close_after_bodies: Option<usize>,
}

impl MockOptions {
    pub fn greeting(mut self, g: &[u8]) -> Self {
        self.greeting = Some(g.to_vec());
        self
    }
    pub fn auth(mut self, m: AuthMode) -> Self {
        self.auth_mode = m;
        self
    }
    pub fn bytewise(mut self) -> Self {
        self.bytewise = true;
        self
    }
    pub fn delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }
    pub fn drop_after(mut self, n: usize) -> Self {
        self.drop_after_bytes = Some(n);
        self
    }
    pub fn close_after_bodies(mut self, n: usize) -> Self {
        self.close_after_bodies = Some(n);
        self
    }
}

/// Server state shared across every accepted connection.
pub struct MockServer {
    articles: HashMap<String, MockArticle>,
    options: MockOptions,
}

impl MockServer {
    pub fn new() -> Self {
        Self {
            articles: HashMap::new(),
            options: MockOptions::default(),
        }
    }
    pub fn with_options(options: MockOptions) -> Self {
        Self {
            articles: HashMap::new(),
            options,
        }
    }

    /// Register an article. `wire_body` should be the dot-stuffed body lines
    /// (each line CRLF-terminated, `..` doubling applied) without the final
    /// `.\r\n` terminator — the harness appends the terminator.
    pub fn add_article(mut self, id: &str, wire_body: Vec<u8>) -> Self {
        self.articles.insert(
            id.to_string(),
            MockArticle {
                wire_body,
                missing: false,
            },
        );
        self
    }

    /// Mark an article as missing on *this* server.
    pub fn add_missing(mut self, id: &str) -> Self {
        self.articles.insert(
            id.to_string(),
            MockArticle {
                wire_body: Vec::new(),
                missing: true,
            },
        );
        self
    }

    /// Bind and spawn the accept loop, returning the bound address.
    pub async fn spawn(&self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let articles = self.articles.clone();
        let options = self.options.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let articles = articles.clone();
                let options = options.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, articles, options).await;
                });
            }
        });
        addr
    }
}

/// dot-stuff a raw body (bytes with `\n` line endings) for the wire.
pub fn dot_stuff(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for line in raw.split(|&b| b == b'\n') {
        for (i, b) in line.iter().enumerate() {
            // Dot at start of a line is doubled.
            if *b == b'.' && (i == 0) {
                out.push(b'.');
            }
            out.push(*b);
        }
        out.push(b'\n');
    }
    out
}

async fn write_alls(
    w: &mut (impl AsyncWriteExt + Unpin),
    data: &[u8],
    options: &MockOptions,
) -> std::io::Result<()> {
    if options.bytewise {
        for b in data {
            w.write_all(&[*b]).await?;
            tokio::time::sleep(Duration::from_micros(50)).await;
        }
    } else {
        w.write_all(data).await?;
    }
    Ok(())
}

async fn handle_conn(
    sock: TcpStream,
    articles: HashMap<String, MockArticle>,
    options: MockOptions,
) -> std::io::Result<()> {
    let (rd, mut wr) = split(sock);
    let mut reader = BufReader::new(rd);
    let mut written: usize = 0;
    let mut bodies_served: usize = 0;

    macro_rules! send {
        ($data:expr) => {{
            let data: &[u8] = $data;
            if let Some(drop) = options.drop_after_bytes {
                if written + data.len() > drop {
                    let n = drop.saturating_sub(written);
                    let _ = wr.write_all(&data[..n]).await;
                    return Ok(());
                }
            }
            if let Some(d) = options.delay {
                tokio::time::sleep(d).await;
            }
            write_alls(&mut wr, data, &options).await?;
            written += data.len();
        }};
    }

    let greeting = options
        .greeting
        .clone()
        .unwrap_or_else(|| b"200 turbonzb-mock ready\r\n".to_vec());
    send!(&greeting);

    let mut line = String::new();

    loop {
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return Ok(()),
            _ => {}
        }
        let cmd = line.trim().to_string();
        line.clear();
        if cmd.is_empty() {
            continue;
        }

        if cmd.starts_with("AUTHINFO USER") {
            match options.auth_mode {
                AuthMode::Accept => send!(b"281 auth accepted\r\n"),
                AuthMode::Challenge => send!(b"381 send password\r\n"),
                AuthMode::Reject => send!(b"502 permission denied\r\n"),
            }
        } else if cmd.starts_with("AUTHINFO PASS") {
            match options.auth_mode {
                AuthMode::Accept | AuthMode::Challenge => {
                    send!(b"281 auth accepted\r\n")
                }
                AuthMode::Reject => send!(b"502 permission denied\r\n"),
            }
        } else if cmd.starts_with("STAT") {
            let id = cmd
                .trim_start_matches("STAT")
                .trim()
                .trim_matches('<')
                .trim_matches('>');
            if let Some(a) = articles.get(id) {
                if a.missing {
                    send!(b"430 no such article\r\n");
                } else {
                    send!(b"223 0 <ok>\r\n");
                }
            } else {
                send!(b"430 no such article\r\n");
            }
        } else if cmd.starts_with("BODY") {
            let id = cmd
                .trim_start_matches("BODY")
                .trim()
                .trim_matches('<')
                .trim_matches('>');
            if let Some(a) = articles.get(id) {
                if a.missing {
                    send!(b"430 no such article\r\n");
                } else {
                    send!(b"222 body follows\r\n");
                    send!(&a.wire_body);
                    send!(b".\r\n");
                    bodies_served += 1;
                    if let Some(limit) = options.close_after_bodies {
                        if bodies_served >= limit {
                            return Ok(());
                        }
                    }
                }
            } else {
                send!(b"430 no such article\r\n");
            }
        } else if cmd == "QUIT" {
            send!(b"205 bye\r\n");
            return Ok(());
        } else if cmd == "NOOP" {
            send!(b"200 ok\r\n");
        } else {
            send!(b"500 unrecognized\r\n");
        }
    }
}
