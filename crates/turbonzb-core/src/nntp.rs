//! Async NNTP client (tokio) with implicit TLS via rustls (port 563) and
//! plaintext (port 119), AUTHINFO USER/PASS, and BODY/STAT commands.
//!
//! This is the transport layer for the downloader engine. One [`NntpClient`]
//! owns a single connection; the engine manages a pool of these per server.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::{CoreError, Result};
use crate::yenc;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Configuration for a single NNTP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port (563 for implicit TLS, 119 for plaintext, 80 for some
    /// providers' plaintext-over-80).
    pub port: u16,
    /// Whether to upgrade to TLS immediately on connect (implicit TLS).
    pub tls: bool,
    /// Username for AUTHINFO USER/PASS. If empty, no auth is attempted.
    pub user: Option<String>,
    /// Password for AUTHINFO USER/PASS.
    pub password: Option<String>,
    /// Maximum simultaneous connections the engine may open to this server.
    pub max_connections: u32,
    /// Priority for fallback ordering (lower = tried first).
    pub priority: u32,
}

impl ServerConfig {
    /// A plaintext localhost:119 server with no auth, for tests.
    pub fn localhost() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 119,
            tls: false,
            user: None,
            password: None,
            max_connections: 4,
            priority: 0,
        }
    }
}

/// A connected NNTP session. Owns the underlying transport (TCP or TCP+TLS).
pub struct NntpClient {
    reader: Transport,
    /// Buffer reused for response lines.
    line_buf: Vec<u8>,
}

impl std::fmt::Debug for NntpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NntpClient").finish()
    }
}

/// The underlying transport: plaintext TCP or TLS-over-TCP. The TLS variant
/// is boxed to keep the enum size balanced (the TLS stream carries extra
/// state that the plain TCP stream does not).
enum Transport {
    Plain(BufReader<TcpStream>),
    Tls(Box<BufReader<tokio_rustls::client::TlsStream<TcpStream>>>),
}

impl Transport {
    /// Bound every transport operation. A NAT box or server that wedges a
    /// connection mid-article would otherwise park the read forever —
    /// with N workers this stalls the whole job with no error. On
    /// timeout the io error propagates, the caller drops the connection
    /// and requeues the segment.
    async fn read_until(&mut self, byte: u8, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        let timed_out = |_scope| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "NNTP read timed out (connection wedged?)",
            )
        };
        match self {
            Transport::Plain(r) => {
                match tokio::time::timeout(READ_TIMEOUT, r.read_until(byte, buf)).await {
                    Ok(result) => result,
                    Err(_) => Err(timed_out(())),
                }
            }
            Transport::Tls(r) => {
                match tokio::time::timeout(READ_TIMEOUT, r.read_until(byte, buf)).await {
                    Ok(result) => result,
                    Err(_) => Err(timed_out(())),
                }
            }
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let timed_out = |_scope| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "NNTP write timed out (connection wedged?)",
            )
        };
        match self {
            Transport::Plain(r) => {
                match tokio::time::timeout(WRITE_TIMEOUT, r.get_mut().write_all(data)).await {
                    Ok(result) => result,
                    Err(_) => Err(timed_out(())),
                }
            }
            Transport::Tls(r) => {
                match tokio::time::timeout(WRITE_TIMEOUT, r.get_mut().write_all(data)).await {
                    Ok(result) => result,
                    Err(_) => Err(timed_out(())),
                }
            }
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(r) => r.get_mut().flush().await,
            Transport::Tls(r) => r.get_mut().flush().await,
        }
    }
}

/// First three digits of an NNTP response, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseCode(pub u16);

impl ResponseCode {
    pub fn parse(line: &str) -> Option<Self> {
        let digits = line.get(..3)?;
        let n: u16 = digits.parse().ok()?;
        Some(Self(n))
    }

    pub fn class(self) -> u16 {
        self.0 / 100
    }
}

/// A fetched article body: the raw bytes returned by the `BODY` command,
/// already dot-unstuffed (lines starting with `..` collapsed to `.`) and with
/// the terminating `.\r\n` removed.
#[derive(Debug, Clone)]
pub struct ArticleBody {
    pub bytes: Vec<u8>,
}

/// Result of a `STAT` lookup: the server's response line tells us whether the
/// article exists (223) or not (423/430).
#[derive(Debug, Clone)]
pub enum StatResult {
    /// Article exists; server returned `223 N message-id`.
    Present,
    /// Article not found on this server (423 no such article, 430 no such
    /// article in this group — both mean "try the next server").
    Missing,
}

impl NntpClient {
    /// Open a new connection to `cfg` and perform the greeting + AUTHINFO
    /// handshake.
    pub async fn connect(cfg: &ServerConfig) -> Result<Self> {
        let connect = TcpStream::connect((cfg.host.as_str(), cfg.port));
        let stream = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(result) => {
                result.map_err(|e| CoreError::NntpConnect(format!("{}: {e}", cfg.host)))?
            }
            Err(_) => {
                return Err(CoreError::NntpConnect(format!(
                    "{}: connect timed out",
                    cfg.host
                )));
            }
        };
        // Disable Nagle's algorithm — NNTP is request/response and each
        // command is small. Without TCP_NODELAY, the OS buffers the tiny
        // `BODY <id>\r\n` command behind Nagle, adding up to 40ms latency
        // per article (the classic "Nagle + delayed ACK" stall).
        let _ = stream.set_nodelay(true);

        // A 256KB read buffer amortizes TLS record overhead and reduces
        // syscall count for large article bodies (~500KB each). The default
        // 8KB BufReader would do ~60 read() syscalls per article; 256KB
        // does 2-3.
        const READ_BUF: usize = 256 * 1024;

        let transport = if cfg.tls {
            let connector = build_tls_connector()?;
            let server_name = ServerName::try_from(cfg.host.clone())
                .map_err(|e| CoreError::NntpConnect(format!("bad host: {e}")))?;
            let handshake = connector.connect(server_name, stream);
            let tls_stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
                Ok(result) => result.map_err(|e| CoreError::NntpConnect(format!("TLS: {e}")))?,
                Err(_) => {
                    return Err(CoreError::NntpConnect(
                        "TLS handshake timed out".to_string(),
                    ));
                }
            };
            Transport::Tls(Box::new(BufReader::with_capacity(READ_BUF, tls_stream)))
        } else {
            Transport::Plain(BufReader::with_capacity(READ_BUF, stream))
        };

        let mut client = Self {
            reader: transport,
            line_buf: Vec::with_capacity(512),
        };

        // Read the server greeting (200/201 posting ok/not allowed).
        let greeting = client.read_response_line().await?;
        match ResponseCode::parse(&greeting) {
            Some(c) if c.class() == 2 => {}
            Some(_) => {
                return Err(CoreError::NntpConnect(format!(
                    "server greeting rejected: {greeting}"
                )));
            }
            None => return Err(CoreError::NntpConnect(format!("bad greeting: {greeting}"))),
        }

        if let (Some(user), Some(pass)) = (cfg.user.as_deref(), cfg.password.as_deref()) {
            client.authinfo(user, pass).await?;
        }

        Ok(client)
    }

    /// Send `AUTHINFO USER` / `AUTHINFO PASS`.
    async fn authinfo(&mut self, user: &str, pass: &str) -> Result<()> {
        self.send_cmd(&format!("AUTHINFO USER {user}")).await?;
        let line = self.read_response_line().await?;
        match ResponseCode::parse(&line) {
            Some(c) if c.0 == 381 => {
                // 381 = send password.
            }
            Some(c) if c.0 == 281 => return Ok(()), // No password needed.
            _ => return Err(CoreError::NtpAuthFailed),
        }
        self.send_cmd(&format!("AUTHINFO PASS {pass}")).await?;
        let line = self.read_response_line().await?;
        match ResponseCode::parse(&line) {
            Some(c) if c.0 == 281 => Ok(()),
            _ => Err(CoreError::NtpAuthFailed),
        }
    }

    /// Fetch the body of an article by message-id via `BODY <id>`.
    ///
    /// Returns the article bytes (dot-unstuffed) on success, or
    /// `StatResult::Missing` if the server reports 430/423.
    pub async fn body(
        &mut self,
        message_id: &str,
    ) -> Result<std::result::Result<ArticleBody, StatResult>> {
        self.send_body(message_id).await?;
        self.read_body_response().await
    }

    /// Write a `BODY <id>` command without waiting for the response.
    ///
    /// Used for NNTP command pipelining: write N commands, then read N
    /// responses in order. Servers that support pipelining buffer the
    /// outgoing article bodies and stream them back‑to‑back, keeping the
    /// connection's send pipe full (no per‑article command round‑trip).
    ///
    /// Do NOT call this casually without subsequently reading responses —
    /// an NNTP server is allowed to (and eventually will) stop reading
    /// commands when its input buffer fills.
    pub async fn send_body(&mut self, message_id: &str) -> Result<()> {
        self.send_cmd(&format!("BODY <{message_id}>")).await
    }

    /// Read the response to a previously issued `BODY` command.
    ///
    /// `222` → read and return the body; `423`/`430` → `Missing`;
    /// anything else is an error.
    pub async fn read_body_response(
        &mut self,
    ) -> Result<std::result::Result<ArticleBody, StatResult>> {
        let status = self.read_response_line().await?;
        let code = ResponseCode::parse(&status)
            .ok_or_else(|| CoreError::Nntp(format!("bad response: {status}")))?;
        match code.0 {
            222 => Ok(Ok(ArticleBody {
                bytes: self.read_dot_body().await?,
            })),
            423 | 430 => Ok(Err(StatResult::Missing)),
            _ => Err(CoreError::Nntp(format!("BODY failed: {status}"))),
        }
    }

    /// Read the response to a previously issued `BODY` command, decoding the
    /// article straight into a yEnc [`crate::yenc::DecodedPart`] as the dot
    /// body streams in — no intermediate encoded-body `Vec` is ever built
    /// (single-copy path, Pillar 1b).
    ///
    /// `222` → return the decoded part; `423`/`430` → `Missing`; anything
    /// else is an error.
    pub async fn read_body_decoded(
        &mut self,
    ) -> Result<std::result::Result<yenc::DecodedPart, StatResult>> {
        let status = self.read_response_line().await?;
        let code = ResponseCode::parse(&status)
            .ok_or_else(|| CoreError::Nntp(format!("bad response: {status}")))?;
        match code.0 {
            222 => {
                let mut dec = yenc::Decoder::new();
                loop {
                    self.line_buf.clear();
                    let n = self
                        .reader
                        .read_until(b'\n', &mut self.line_buf)
                        .await
                        .map_err(CoreError::from)?;
                    if n == 0 {
                        return Err(CoreError::Nntp("connection closed mid-body".into()));
                    }
                    // Terminator: a line of just `.` (optionally with CRLF).
                    let trimmed = self
                        .line_buf
                        .strip_suffix(b"\r\n")
                        .or_else(|| self.line_buf.strip_suffix(b"\n"))
                        .unwrap_or(&self.line_buf);
                    if trimmed == b"." {
                        break;
                    }
                    // Dot-unstuffing: a leading `..` collapses to `.`.
                    let line = if let Some(rest) = trimmed.strip_prefix(b"..") {
                        let mut buf = Vec::with_capacity(rest.len() + 1);
                        buf.push(b'.');
                        buf.extend_from_slice(rest);
                        buf
                    } else {
                        trimmed.to_vec()
                    };
                    dec.push_line(&line)?;
                }
                Ok(Ok(dec.finish()?))
            }
            423 | 430 => Ok(Err(StatResult::Missing)),
            _ => Err(CoreError::Nntp(format!("BODY failed: {status}"))),
        }
    }

    /// Check article presence via `STAT <id>` (cheaper than BODY: no payload).
    pub async fn stat(&mut self, message_id: &str) -> Result<StatResult> {
        self.send_cmd(&format!("STAT <{message_id}>")).await?;
        let line = self.read_response_line().await?;
        let code = ResponseCode::parse(&line)
            .ok_or_else(|| CoreError::Nntp(format!("bad response: {line}")))?;
        match code.0 {
            223 => Ok(StatResult::Present),
            423 | 430 => Ok(StatResult::Missing),
            _ => Err(CoreError::Nntp(format!("STAT failed: {line}"))),
        }
    }

    /// Send a `NOOP` — a cheap keep-alive so the provider doesn't close
    /// connections that sit idle in the pool.
    pub async fn noop(&mut self) -> Result<()> {
        self.send_cmd("NOOP").await?;
        let line = self.read_response_line().await?;
        let code = ResponseCode::parse(&line)
            .ok_or_else(|| CoreError::Nntp(format!("bad response: {line}")))?;
        if code.0 == 200 || code.class() == 2 {
            Ok(())
        } else {
            Err(CoreError::Nntp(format!("NOOP failed: {line}")))
        }
    }

    /// Send a single command line (CRLF-terminated). The command and CRLF
    /// are combined into a single `write_all` to produce one TLS record and
    /// one packet — splitting them caused an extra TLS record and potential
    /// Nagle/delayed-ACK interaction.
    async fn send_cmd(&mut self, cmd: &str) -> Result<()> {
        let mut buf = Vec::with_capacity(cmd.len() + 2);
        buf.extend_from_slice(cmd.as_bytes());
        buf.extend_from_slice(b"\r\n");
        self.reader.write_all(&buf).await.map_err(CoreError::from)?;
        self.reader.flush().await.map_err(CoreError::from)?;
        tracing::trace!(cmd, "nntp ->");
        Ok(())
    }

    /// Read one CRLF-terminated response line into a String.
    async fn read_response_line(&mut self) -> Result<String> {
        self.line_buf.clear();
        self.reader
            .read_until(b'\n', &mut self.line_buf)
            .await
            .map_err(CoreError::from)?;
        if self.line_buf.is_empty() {
            return Err(CoreError::Nntp("connection closed mid-response".into()));
        }
        // Strip trailing \r\n or \n.
        while let Some(&last) = self.line_buf.last() {
            if last == b'\n' || last == b'\r' {
                self.line_buf.pop();
            } else {
                break;
            }
        }
        let line = String::from_utf8_lossy(&self.line_buf).into_owned();
        tracing::trace!(line = %line, "nntp <-");
        Ok(line)
    }

    /// Read a dot-stuffed multi-line response body (after the initial `222`
    /// status line), until the terminating `.\r\n`. Returns the unstuffed
    /// bytes.
    async fn read_dot_body(&mut self) -> Result<Vec<u8>> {
        // Most article bodies are ~500KB (yEnc-encoded segments). Pre-
        // allocating avoids repeated Vec growth (doubling from 0 would
        // involve ~19 reallocations for a 500KB body).
        let mut out = Vec::with_capacity(512 * 1024);
        loop {
            self.line_buf.clear();
            let n = self
                .reader
                .read_until(b'\n', &mut self.line_buf)
                .await
                .map_err(CoreError::from)?;
            if n == 0 {
                return Err(CoreError::Nntp("connection closed mid-body".into()));
            }
            // A line consisting of just `.` (optionally with CRLF) is the
            // terminator.
            let trimmed = self
                .line_buf
                .strip_suffix(b"\r\n")
                .or_else(|| self.line_buf.strip_suffix(b"\n"))
                .unwrap_or(&self.line_buf);
            if trimmed == b"." {
                break;
            }
            // Dot-unstuffing: a leading `..` collapses to `.`.
            if let Some(rest) = trimmed.strip_prefix(b"..") {
                out.extend_from_slice(b".");
                out.extend_from_slice(rest);
            } else {
                out.extend_from_slice(trimmed);
            }
            // Preserve the line break in the decoded output — the yEnc decoder
            // strips CRLF itself, so keeping them here is harmless and matches
            // what the article body actually looks like.
            out.push(b'\r');
            out.push(b'\n');
        }
        Ok(out)
    }
}

/// Build a `TlsConnector` using the webpki-roots trust store. Self-signed /
/// mismatched certs are *not* accepted — use [`build_insecure_tls_connector`]
/// only for local tests.
fn build_tls_connector() -> Result<TlsConnector> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::client::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Build a TLS connector that accepts any server certificate. For NNTP
/// providers that use private CAs the user didn't install, or for local test
/// servers. **Never use this for untrusted servers.**
#[allow(dead_code)]
pub fn build_insecure_tls_connector() -> Result<TlsConnector> {
    let config = rustls::client::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test(start_paused = true)]
    async fn silent_server_times_out_instead_of_hanging() {
        // A server that accepts but never sends anything used to park the
        // greeting read forever (wedged NAT/server at scale = whole job
        // stalled with no error). Now it must fail fast with a timeout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and deliberately never respond.
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut cfg = ServerConfig::localhost();
        cfg.port = addr.port();
        let start = tokio::time::Instant::now();
        let err = NntpClient::connect(&cfg).await.expect_err("must time out");
        assert!(
            err.to_string().to_lowercase().contains("timed out"),
            "got: {err}"
        );
        assert!(start.elapsed() >= std::time::Duration::from_secs(120));
    }

    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// A minimal fake NNTP server that speaks just enough of the protocol for
    /// our client tests: greeting, AUTHINFO, STAT, BODY with dot-stuffing.
    async fn spawn_fake_nntp() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = tokio::io::split(sock);
            let mut reader = BufReader::new(reader);
            writer
                .write_all(b"200 turbonzb-fake ready\r\n")
                .await
                .unwrap();

            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap() == 0 {
                    return;
                }
                let cmd = line.trim().to_string();
                if cmd.is_empty() {
                    continue;
                }
                if cmd.starts_with("AUTHINFO USER") {
                    writer.write_all(b"381 send password\r\n").await.unwrap();
                } else if cmd.starts_with("AUTHINFO PASS") {
                    writer.write_all(b"281 auth accepted\r\n").await.unwrap();
                } else if cmd.starts_with("STAT") {
                    if cmd.contains("missing@x") {
                        writer.write_all(b"430 no such article\r\n").await.unwrap();
                    } else {
                        writer.write_all(b"223 0 <exists@x>\r\n").await.unwrap();
                    }
                } else if cmd.starts_with("BODY") {
                    if cmd.contains("missing@x") {
                        writer.write_all(b"430 no such article\r\n").await.unwrap();
                    } else if cmd.contains("dotted@x") {
                        // A dot-stuffed yEnc article: payload byte 4 encodes
                        // to '.' which is doubled on the wire as '..'.
                        writer.write_all(b"222 body follows\r\n").await.unwrap();
                        writer
                            .write_all(b"=ybegin size=1 name=t\r\n")
                            .await
                            .unwrap();
                        writer.write_all(b"..\r\n").await.unwrap();
                        writer.write_all(b"=yend size=1\r\n").await.unwrap();
                        writer.write_all(b".\r\n").await.unwrap();
                    } else {
                        writer.write_all(b"222 body follows\r\n").await.unwrap();
                        writer.write_all(b"hello-body\r\n").await.unwrap();
                        writer.write_all(b".\r\n").await.unwrap();
                    }
                } else if cmd == "QUIT" {
                    writer.write_all(b"205 bye\r\n").await.unwrap();
                    return;
                } else {
                    writer.write_all(b"500 unknown\r\n").await.unwrap();
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn connects_and_stats() {
        let addr = spawn_fake_nntp().await;
        let mut cfg = ServerConfig::localhost();
        cfg.port = addr.port();
        let mut c = NntpClient::connect(&cfg).await.unwrap();
        assert!(matches!(
            c.stat("exists@x").await.unwrap(),
            StatResult::Present
        ));
        assert!(matches!(
            c.stat("missing@x").await.unwrap(),
            StatResult::Missing
        ));
    }

    #[tokio::test]
    async fn authinfo_flow() {
        let addr = spawn_fake_nntp().await;
        let mut cfg = ServerConfig::localhost();
        cfg.port = addr.port();
        cfg.user = Some("u".into());
        cfg.password = Some("p".into());
        let mut c = NntpClient::connect(&cfg).await.unwrap();
        // If we got here, auth succeeded.
        assert!(matches!(
            c.stat("exists@x").await.unwrap(),
            StatResult::Present
        ));
    }

    #[tokio::test]
    async fn body_fetches_and_unstuffs() {
        let addr = spawn_fake_nntp().await;
        let mut cfg = ServerConfig::localhost();
        cfg.port = addr.port();
        let mut c = NntpClient::connect(&cfg).await.unwrap();
        let body = c.body("dotted@x").await.unwrap().unwrap();
        let s = String::from_utf8_lossy(&body.bytes);
        assert!(
            s.contains("."),
            "dot-unstuffing should produce a single dot, got: {s}"
        );
        assert!(!s.contains(".."));
    }

    #[tokio::test]
    async fn read_body_decoded_unstuffs_and_decodes() {
        // `dotted@x` returns a dot-stuffed yEnc article: the payload byte 4
        // encodes to '.' which is doubled on the wire as `..` → must be
        // unstuffed to a single byte before decode.
        let addr = spawn_fake_nntp().await;
        let mut cfg = ServerConfig::localhost();
        cfg.port = addr.port();
        let mut c = NntpClient::connect(&cfg).await.unwrap();
        c.send_body("dotted@x").await.unwrap();
        let part = c.read_body_decoded().await.unwrap().unwrap();
        // The unstuffed `..` → `.` decodes (wrapping) to byte 4.
        assert_eq!(part.data, vec![4u8], "dot must be unstuffed exactly once");
        assert_eq!(part.end, 1);
        assert!(part.crc_unknown);
    }

    #[tokio::test]
    async fn body_returns_missing_on_430() {
        let addr = spawn_fake_nntp().await;
        let mut cfg = ServerConfig::localhost();
        cfg.port = addr.port();
        let mut c = NntpClient::connect(&cfg).await.unwrap();
        let res = c.body("missing@x").await.unwrap();
        assert!(matches!(res, Err(StatResult::Missing)));
    }
}
