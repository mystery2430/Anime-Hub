//! Connection-time IP pinning for the desktop site WebView.
//!
//! The navigation guard in `dns.rs` classifies a hostname *before* the WebView
//! connects. That leaves the window the DNS-rebinding attack needs: the
//! WebView resolves the name again on its own stack, and a `fetch`, XHR,
//! iframe or a later navigation is not covered by the top-level callback at
//! all. The only place where the address that is actually dialled can be
//! chosen is the socket.
//!
//! This module is a local HTTP `CONNECT` proxy that the site WebView is
//! configured to use for every request:
//!
//!   * only `CONNECT` is accepted — plain HTTP in absolute form is refused,
//!     because a request line that names its own destination is traffic the
//!     WebView could also send directly;
//!   * the hostname is resolved exactly once, here, and every answer must be
//!     public (see [`is_public_ip`]); an empty, failed or mixed answer is
//!     refused, so a public-then-private rebinding cannot produce a socket;
//!   * the socket is opened to the **classified numeric address**, never to
//!     the name, and the connected peer is checked against that address;
//!   * TLS stays end-to-end: the tunnel is a byte pipe, nothing is
//!     terminated and no certificate check is skipped;
//!   * resources are bounded (connections, resolver work, handshake size and
//!     time) and a refused or failed request never opens an upstream socket.
//!
//! What it does not cover: a WebView that ignores its proxy setting, and any
//! transport the WebView does not send through an HTTP proxy (WebRTC/QUIC
//! UDP). Those are stated in `docs/security-residual-risks.md`; the WebRTC
//! API is additionally removed at document start by `webrtc_guard.js`.

use crate::sites::url_policy::{is_private_host, is_public_ip};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};

/// Bounds for one site session. Production uses [`Limits::DEFAULT`]; the
/// tests shorten the timeouts so a refusal is observable without waiting for
/// a real network timeout.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Concurrent tunnels a single site session may hold. A page opens a
    /// handful of connections per origin; the cap is what turns a hostile
    /// page into a bounded number of sockets instead of an unbounded one.
    max_connections: usize,
    /// Concurrent resolver calls. Lookups run on the blocking pool, so this
    /// also bounds how many pool threads one page can occupy.
    max_resolvers: usize,
    /// How long the request line and headers may take to arrive.
    handshake: Duration,
    /// A resolver answer must arrive within this window or the request is
    /// refused.
    resolve: Duration,
    /// Time allowed to reach the classified address.
    connect: Duration,
}

impl Limits {
    const DEFAULT: Limits = Limits {
        max_connections: 48,
        max_resolvers: 8,
        handshake: Duration::from_secs(10),
        resolve: Duration::from_secs(5),
        connect: Duration::from_secs(10),
    };
}

/// Largest request head accepted, matching what a browser sends for CONNECT.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Which destinations a tunnel may reach.
///
/// Production always uses [`Policy::PUBLIC_ONLY`]. The loopback exception
/// exists only for the tests below: they need a destination that a test can
/// actually listen on, and a private address is exactly what production must
/// refuse. The constructor is `#[cfg(test)]` so no shipped code path can build
/// a permissive policy.
#[derive(Debug, Clone, Copy)]
struct Policy {
    allow_loopback: bool,
}

impl Policy {
    const PUBLIC_ONLY: Policy = Policy {
        allow_loopback: false,
    };

    #[cfg(test)]
    const LOOPBACK_FOR_TESTS: Policy = Policy {
        allow_loopback: true,
    };

    fn allows(self, ip: IpAddr) -> bool {
        if is_public_ip(ip) {
            return true;
        }
        self.allow_loopback && ip.is_loopback()
    }
}

type ResolveFuture = Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, String>> + Send>>;

/// One resolution per request, injectable so tests can script the answers.
trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
}

/// The production resolver: the platform's own name resolution.
struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|_| "çözümleme başarısız".to_string())?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        })
    }
}

/// Everything a connection needs, shared by every accepted socket.
struct Context {
    resolver: Arc<dyn Resolver>,
    policy: Policy,
    limits: Limits,
    resolvers: Arc<Semaphore>,
    requests: Arc<AtomicUsize>,
}

/// A running proxy. Dropping or stopping it closes the listener and every
/// tunnel; the port is never reused by this process afterwards.
pub struct ProxyHandle {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    requests: Arc<AtomicUsize>,
}

impl ProxyHandle {
    /// The loopback port the WebView must be pointed at.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The exact address the listener bound. Test-only: the open path needs
    /// the port, and the address is what makes "loopback only" checkable.
    #[cfg(test)]
    pub fn bind_addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many CONNECT requests have been parsed since this proxy started.
    ///
    /// A WebView that finished loading a page while this is still zero never
    /// used the proxy, which is the fail-open case the caller has to notice.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Stop the listener and tear down open tunnels.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}

/// Start the production proxy on an ephemeral loopback port.
pub async fn start() -> std::io::Result<ProxyHandle> {
    start_with(
        Arc::new(SystemResolver),
        Policy::PUBLIC_ONLY,
        Limits::DEFAULT,
    )
    .await
}

async fn start_with(
    resolver: Arc<dyn Resolver>,
    policy: Policy,
    limits: Limits,
) -> std::io::Result<ProxyHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(limits.max_connections));
    let requests = Arc::new(AtomicUsize::new(0));
    let context = Arc::new(Context {
        resolver,
        policy,
        limits,
        resolvers: Arc::new(Semaphore::new(limits.max_resolvers)),
        requests: requests.clone(),
    });
    let task = tokio::spawn(accept_loop(listener, context, connections, stop_rx));
    Ok(ProxyHandle {
        addr,
        stop: stop_tx,
        task,
        requests,
    })
}

async fn accept_loop(
    listener: TcpListener,
    context: Arc<Context>,
    connections: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
                continue;
            }
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _peer)) = accepted else {
            // A failed accept is not a reason to drop the listener.
            continue;
        };
        // Refuse rather than queue: a page that exceeds the cap must not be
        // able to grow the socket count while it waits.
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            let mut stream = stream;
            let _ = reply(&mut stream, 503, "Service Unavailable").await;
            continue;
        };
        let context = context.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut stream = stream;
            handle_connection(&mut stream, &context, stop).await;
        });
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    context: &Context,
    mut stop: watch::Receiver<bool>,
) {
    let limits = context.limits;
    let Ok(head) = tokio::time::timeout(limits.handshake, read_head(stream)).await else {
        let _ = reply(stream, 408, "Request Timeout").await;
        return;
    };
    let head = match head {
        Ok(head) => head,
        Err(status) => {
            let _ = reply(stream, status, status_text(status)).await;
            return;
        }
    };

    let Some((host, port)) = parse_connect(&head) else {
        let _ = reply(stream, 405, "Method Not Allowed").await;
        return;
    };
    // Counted before the destination decision: this is the proof that the
    // WebView is talking to the proxy at all, not a claim that it was allowed.
    context.requests.fetch_add(1, Ordering::SeqCst);

    let addrs = match classify(&host, port, context).await {
        Ok(addrs) => addrs,
        Err(status) => {
            let _ = reply(stream, status, status_text(status)).await;
            return;
        }
    };

    let Some(upstream) = connect_first(&addrs, port, limits.connect).await else {
        let _ = reply(stream, 502, "Bad Gateway").await;
        return;
    };

    if reply(stream, 200, "Connection Established").await.is_err() {
        return;
    }
    let mut upstream = upstream;
    tokio::select! {
        _ = tokio::io::copy_bidirectional(stream, &mut upstream) => {}
        _ = stop.changed() => {}
    }
}

/// Resolve once and return only addresses the policy allows.
///
/// Any non-public address in the answer refuses the whole request: taking the
/// public ones would make the outcome depend on which address the connect path
/// happened to pick, and that is the choice the attacker wants to influence.
async fn classify(host: &str, port: u16, context: &Context) -> Result<Vec<IpAddr>, u16> {
    if host.is_empty() || host.len() > 253 {
        return Err(400);
    }
    // A literal (`127.0.0.1`, `[::1]`) is classified directly and never
    // reaches the resolver.
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return if context.policy.allows(ip) {
            Ok(vec![ip])
        } else {
            Err(403)
        };
    }
    // Names the string policy already knows are refused without a lookup, so a
    // rebinding answer cannot resurrect `localhost` or `*.local`.
    if is_private_host(host) {
        return Err(403);
    }
    let Ok(_permit) = context.resolvers.clone().try_acquire_owned() else {
        return Err(503);
    };
    let answer = context.resolver.resolve(host.to_string(), port);
    let resolved = match tokio::time::timeout(context.limits.resolve, answer).await {
        Ok(Ok(addrs)) => addrs,
        // A timeout or an error is a refusal: the fallback would be the
        // WebView's own resolver, which is the thing being pinned.
        Ok(Err(_)) | Err(_) => return Err(403),
    };
    if resolved.is_empty() || resolved.iter().any(|ip| !context.policy.allows(*ip)) {
        return Err(403);
    }
    Ok(resolved)
}

async fn connect_first(addrs: &[IpAddr], port: u16, timeout: Duration) -> Option<TcpStream> {
    for ip in addrs {
        let addr = SocketAddr::new(*ip, port);
        let Ok(Ok(stream)) = tokio::time::timeout(timeout, TcpStream::connect(addr)).await else {
            continue;
        };
        // Belt and braces: the socket must have reached the address that was
        // classified, not some other one.
        if let Ok(peer) = stream.peer_addr() {
            if peer.ip() == *ip {
                return Some(stream);
            }
        }
    }
    None
}

fn status_text(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Bad Gateway",
    }
}

async fn reply(stream: &mut TcpStream, status: u16, reason: &str) -> std::io::Result<()> {
    let body =
        format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, u16> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if read == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n".as_slice()) {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(431);
        }
    }
}

/// Parse a CONNECT request head into `(host, port)`.
///
/// `None` means the request is not an acceptable CONNECT and **must not reach
/// the resolver or a socket**: a different method, an unsupported HTTP version,
/// a target that is not `authority-form`, userinfo, a missing/zero/oversized
/// port, or a header block with stray control bytes.
///
/// The grammar is `DnsPinnedProxy.parseConnectHead` / `parseAuthority`, and
/// both suites read `tests/fixtures/connect-request.tsv`, so a rule added to
/// one parser alone fails the other suite.
///
/// What comes from a standard, and what does not:
///
///   * **RFC 9112 §3.1** — "The request method is case-sensitive". `connect`
///     and `Connect` are different methods and are refused. (Round 23: this
///     used to be `eq_ignore_ascii_case`, so the desktop tunnelled a request
///     the RFC does not call CONNECT and Android refused it.)
///   * **RFC 9112 §2.3** — `HTTP-version = HTTP-name "/" DIGIT "." DIGIT`,
///     case-sensitive. The app accepts exactly `HTTP/1.0` and `HTTP/1.1`; a
///     syntactically valid `HTTP/1.2` is **also** refused, because this proxy
///     tunnels 1.x and nothing else. That is an application choice, not an RFC
///     requirement, and it is the same choice on Android.
///   * **RFC 9112 §3.2.3** — `authority-form = uri-host ":" port`, and
///     **RFC 3986 §3.2.3** — `port = *DIGIT`.
///   * **Application restrictions** (deliberately stricter than the RFCs, which
///     would also allow an empty port, `reg-name` sub-delims and
///     percent-encoding): the port is required and at most five digits; a host
///     is printable ASCII with no `@ / ? # \`; percent-encoding is refused
///     because nothing here decodes it; a host is at most 253 bytes with no
///     empty label; brackets only ever wrap a real IPv6 literal.
fn parse_connect(head: &[u8]) -> Option<(String, u16)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let request = lines.next()?;
    let mut parts = request.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    // RFC 9112 §3.1: case-sensitive. `connect` is not the CONNECT method.
    if method != "CONNECT" {
        return None;
    }
    // RFC 9112 §2.3: exact, case-sensitive version. See the doc comment for why
    // `HTTP/1.2` is refused even though its syntax is valid.
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return None;
    }
    // Only printable ASCII can appear in `authority-form`. This one check
    // rejects spaces, every control byte (including a raw HTAB) and every
    // non-ASCII byte, so none of them can reach the resolver.
    if !target.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return None;
    }
    // Delimiters that mean something else in a URL, and percent-encoding that
    // this proxy never decodes.
    if target
        .bytes()
        .any(|b| matches!(b, b'@' | b'/' | b'?' | b'#' | b'\\' | b'%'))
    {
        return None;
    }
    let (host, port) = target.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    if let Some(inner) = host.strip_prefix('[') {
        // `IP-literal`: the brackets have to wrap a real IPv6 address, and
        // nothing else. `classify` would happily classify a bracketed IPv4, but
        // that is not `authority-form`.
        let inner = inner.strip_suffix(']')?;
        if inner.parse::<std::net::Ipv6Addr>().is_err() {
            return None;
        }
        // RFC 4291 text form is at most 45 characters.
        if inner.len() > 45 {
            return None;
        }
    } else {
        // An unbracketed host is a name: no ':' (an IPv6 literal must be
        // bracketed in an authority) and no stray bracket. `foo[bar` used to
        // pass this and was handed to the resolver.
        if host.contains(':') || host.contains('[') || host.contains(']') {
            return None;
        }
        if host.len() > 253 {
            return None;
        }
    }
    // A trailing dot is the root label and is dropped; any other empty label
    // (`foo..bar`, `.foo`) is not a name this proxy resolves.
    let trimmed = host.trim_end_matches('.');
    if trimmed.is_empty() || trimmed.split('.').any(str::is_empty) {
        return None;
    }
    // `port = *DIGIT` (RFC 3986 §3.2.3), at most five of them: `0000443` is not
    // a port the Android parser accepts, and a valid port never needs six.
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    // A header block that carries control characters other than CR/LF/TAB is
    // malformed; refusing keeps the parser from guessing at intent.
    if text
        .bytes()
        .any(|b| b < 0x20 && b != b'\r' && b != b'\n' && b != b'\t')
    {
        return None;
    }
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    /// A resolver whose answers are scripted by the test.
    struct ScriptedResolver {
        answers: Mutex<VecDeque<Result<Vec<IpAddr>, String>>>,
        calls: AtomicUsize,
    }

    impl ScriptedResolver {
        fn new(answers: Vec<Result<Vec<IpAddr>, String>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Resolver for ScriptedResolver {
        fn resolve(&self, _host: String, _port: u16) -> ResolveFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = {
                let mut answers = self.answers.lock().unwrap();
                answers.pop_front()
            };
            Box::pin(async move { next.unwrap_or_else(|| Err("script exhausted".to_string())) })
        }
    }

    /// A loopback listener standing in for a site origin: it echoes one line.
    async fn origin() -> (u16, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(&buf[..n]).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (port, task)
    }

    /// Connect to the proxy itself.
    async fn dial(port: u16) -> TcpStream {
        TcpStream::connect(("127.0.0.1", port)).await.unwrap()
    }

    /// Send one CONNECT and return the status line.
    async fn connect_via(proxy_port: u16, authority: &str) -> String {
        let mut stream = dial(proxy_port).await;
        let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 256];
        let n = stream.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        text.lines().next().unwrap_or("").to_string()
    }

    /// Production bounds, shortened so a refusal is observable immediately.
    fn test_limits() -> Limits {
        Limits {
            max_connections: 8,
            max_resolvers: 2,
            handshake: Duration::from_millis(400),
            resolve: Duration::from_millis(400),
            connect: Duration::from_millis(400),
        }
    }

    fn public_answer() -> Result<Vec<IpAddr>, String> {
        // The classifier allows 192.0.0.9 (an IANA exception that is globally
        // reachable), and nothing listens on it: a dial fails within the test
        // timeout instead of depending on the runner's egress policy.
        Ok(vec![IpAddr::from([192, 0, 0, 9])])
    }

    #[tokio::test]
    async fn a_public_answer_is_tunnelled_to_the_classified_address() {
        let (origin_port, origin_task) = origin().await;
        let resolver = ScriptedResolver::new(vec![Ok(vec![IpAddr::from([127, 0, 0, 1])])]);
        let proxy = start_with(resolver.clone(), Policy::LOOPBACK_FOR_TESTS, test_limits())
            .await
            .expect("proxy");

        let mut stream = dial(proxy.port()).await;
        let authority = format!("allowed.test:{origin_port}");
        let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut head = vec![0u8; 128];
        let n = stream.read(&mut head).await.unwrap();
        let head = String::from_utf8_lossy(&head[..n]).to_string();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");

        // The tunnel is a byte pipe to the origin that was dialled by address.
        stream.write_all(b"ping\n").await.unwrap();
        let mut echoed = vec![0u8; 16];
        let n = stream.read(&mut echoed).await.unwrap();
        assert_eq!(&echoed[..n], b"ping\n");
        assert_eq!(origin_task.await.unwrap(), "ping\n");
        assert_eq!(resolver.calls(), 1, "one CONNECT resolves the name once");
        proxy.stop();
    }

    /// The rebinding case: the first answer is public, the second is private.
    /// The second CONNECT must not open a socket to the private address.
    #[tokio::test]
    async fn a_rebinding_second_answer_never_reaches_the_private_address() {
        let (catcher_port, catcher) = {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            (port, listener)
        };
        let resolver = ScriptedResolver::new(vec![
            public_answer(),
            Ok(vec![IpAddr::from([127, 0, 0, 1])]),
        ]);
        // Production policy: loopback is refused, exactly as in a real rebinding.
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");

        // First request gets the public answer; it may or may not connect (the
        // address is TEST-NET and not routable here), which is irrelevant.
        let first = connect_via(proxy.port(), &format!("rebind.test:{catcher_port}")).await;
        assert!(
            !first.contains(" 200 "),
            "public TEST-NET must not tunnel: {first}"
        );

        // Second request re-resolves and gets the private address: refused.
        let second = connect_via(proxy.port(), &format!("rebind.test:{catcher_port}")).await;
        assert!(second.starts_with("HTTP/1.1 403"), "{second}");
        assert_eq!(
            resolver.calls(),
            2,
            "each CONNECT resolves once, never cached"
        );

        // The private address was never dialled: the catcher accepts nothing.
        let accepted = tokio::time::timeout(Duration::from_millis(300), catcher.accept()).await;
        if let Ok(accepted) = accepted {
            panic!("a private destination was dialled: {accepted:?}");
        }
        proxy.stop();
    }

    #[tokio::test]
    async fn a_mixed_answer_is_refused_as_a_whole() {
        let (catcher_port, catcher) = {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            (port, listener)
        };
        let resolver = ScriptedResolver::new(vec![Ok(vec![
            IpAddr::from([192, 0, 0, 9]),
            IpAddr::from([127, 0, 0, 1]),
        ])]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");

        let status = connect_via(proxy.port(), &format!("mixed.test:{catcher_port}")).await;
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        let accepted = tokio::time::timeout(Duration::from_millis(300), catcher.accept()).await;
        if let Ok(accepted) = accepted {
            panic!("a mixed answer opened a socket: {accepted:?}");
        }
        proxy.stop();
    }

    #[tokio::test]
    async fn private_and_empty_answers_are_refused_without_a_socket() {
        let resolver = ScriptedResolver::new(vec![
            Ok(vec![IpAddr::from([10, 0, 0, 5])]),
            Ok(vec![]),
            Err("NXDOMAIN".to_string()),
        ]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        for _ in 0..3 {
            let status = connect_via(proxy.port(), "sink.test:443").await;
            assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        }
        assert_eq!(resolver.calls(), 3);
        proxy.stop();
    }

    #[tokio::test]
    async fn a_private_literal_is_refused_without_a_lookup() {
        let resolver = ScriptedResolver::new(vec![public_answer()]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        for authority in ["127.0.0.1:443", "10.0.0.1:80", "[::1]:443", "localhost:80"] {
            let status = connect_via(proxy.port(), authority).await;
            assert!(status.starts_with("HTTP/1.1 403"), "{authority}: {status}");
        }
        assert_eq!(
            resolver.calls(),
            0,
            "literals and local names skip the resolver"
        );
        proxy.stop();
    }

    #[tokio::test]
    async fn anything_but_connect_is_refused() {
        let resolver = ScriptedResolver::new(vec![public_answer()]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        let mut stream = dial(proxy.port()).await;
        stream
            .write_all(b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 128];
        let n = stream.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(head.starts_with("HTTP/1.1 405"), "{head}");
        assert_eq!(resolver.calls(), 0, "no lookup without a CONNECT");
        proxy.stop();
    }

    #[tokio::test]
    async fn an_oversized_head_is_refused() {
        let resolver = ScriptedResolver::new(vec![public_answer()]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        let mut stream = dial(proxy.port()).await;
        let mut request = String::from("CONNECT example.test:443 HTTP/1.1\r\n");
        while request.len() < MAX_HEAD_BYTES + 64 {
            request.push_str("X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        let _ = stream.write_all(request.as_bytes()).await;
        let mut buf = vec![0u8; 256];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            head.starts_with("HTTP/1.1 431") || head.is_empty(),
            "oversized head must not be parsed: {head}"
        );
        proxy.stop();
    }

    #[tokio::test]
    async fn a_failed_connect_has_no_fallback() {
        // A public answer nobody listens on: the proxy answers 502 and stops.
        let resolver = ScriptedResolver::new(vec![public_answer()]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        let started = Instant::now();
        let status = connect_via(proxy.port(), "unreachable.test:9").await;
        assert!(status.starts_with("HTTP/1.1 502"), "{status}");
        assert!(started.elapsed() < test_limits().connect + Duration::from_secs(2));
        proxy.stop();
    }

    #[tokio::test]
    async fn stop_closes_the_listener_and_open_tunnels() {
        let (origin_port, origin_task) = origin().await;
        let resolver = ScriptedResolver::new(vec![Ok(vec![IpAddr::from([127, 0, 0, 1])])]);
        let proxy = start_with(resolver, Policy::LOOPBACK_FOR_TESTS, test_limits())
            .await
            .expect("proxy");
        let port = proxy.port();

        let mut stream = dial(port).await;
        let authority = format!("allowed.test:{origin_port}");
        let request = format!("CONNECT {authority} HTTP/1.1\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 128];
        let n = stream.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));

        proxy.stop();
        // The tunnel is torn down: the reading side sees EOF rather than more
        // bytes (the origin echoes once, so nothing else is expected).
        let _ = origin_task.await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut rest)).await;

        // The port no longer accepts.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let closed = TcpStream::connect(("127.0.0.1", port)).await;
        assert!(closed.is_err(), "the stopped listener must not accept");
    }

    #[tokio::test]
    async fn the_request_counter_separates_bypass_from_refusal() {
        let resolver = ScriptedResolver::new(vec![Ok(vec![IpAddr::from([127, 0, 0, 1])])]);
        let proxy = start_with(resolver, Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        assert_eq!(proxy.requests(), 0);
        // A refused CONNECT still proves the WebView used the proxy.
        let status = connect_via(proxy.port(), "sink.test:443").await;
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        assert_eq!(proxy.requests(), 1);
        proxy.stop();
    }

    #[tokio::test]
    async fn every_start_binds_its_own_port() {
        let resolver = ScriptedResolver::new(vec![public_answer(), public_answer()]);
        let first = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        let first_port = first.port();
        first.stop();
        let second = start_with(resolver, Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        assert_ne!(first_port, second.port());
        second.stop();
    }

    #[tokio::test]
    async fn the_listener_binds_loopback_only() {
        let resolver = ScriptedResolver::new(vec![]);
        let proxy = start_with(resolver, Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        let addr = proxy.bind_addr();
        // `start_with` binds ("127.0.0.1", 0): the OS picks the port and the
        // address is loopback, so nothing outside this machine can dial it.
        // That is the whole of the network-side boundary (P3): it is not a
        // client authentication.
        assert!(addr.ip().is_loopback(), "bound {addr}");
        assert_ne!(addr.port(), 0);
        proxy.stop();
    }

    /// P3, pinning an **open** residual: the listener has no notion of a
    /// trusted client.
    ///
    /// Any process on the machine can dial it and ask for a tunnel; only the
    /// destination policy bounds where that tunnel may go. The test exists so
    /// that adding client authentication later has to change this pin and the
    /// risk document together, instead of silently contradicting them.
    #[tokio::test]
    async fn a_local_client_is_served_without_authentication() {
        let (origin_port, origin_task) = origin().await;
        let resolver = ScriptedResolver::new(vec![Ok(vec![IpAddr::from([127, 0, 0, 1])])]);
        let proxy = start_with(resolver, Policy::LOOPBACK_FOR_TESTS, test_limits())
            .await
            .expect("proxy");
        // A bare TCP client — no WebView, no credentials, no handshake beyond
        // the CONNECT line — is tunnelled exactly like the site WebView would
        // be. Nothing in the request identifies the caller.
        let status = connect_via(proxy.port(), &format!("allowed.test:{origin_port}")).await;
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        proxy.stop();
        let _ = origin_task.await;
    }

    /// The CONNECT grammar shared with the Android proxy.
    ///
    /// One fixture, two row kinds, and each row runs the **production** parser
    /// on the side that reads it — no test-local reimplementation:
    ///
    ///   * `request`   — the whole line, through `parse_connect`;
    ///   * `authority` — the same line wrapped as `CONNECT <a> HTTP/1.1`.
    ///
    /// `DnsPinnedProxyTest` reads the identical file through `parseConnectHead`
    /// and `parseAuthority`, so a method, version, character, bracket or port
    /// rule added to one parser alone fails the other suite. `accept` means the
    /// parser accepts the line; the destination policy still runs afterwards.
    #[test]
    fn connect_request_vectors_match_the_android_parser() {
        let fixture = include_str!("../../../tests/fixtures/connect-request.tsv");
        let (mut requests, mut authorities) = (0usize, 0usize);
        for raw in fixture.lines() {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(
                cols.len(),
                3,
                "a vector is <kind>\\t<value>\\t<decision>: {line}"
            );
            let head = match cols[0] {
                "request" => {
                    requests += 1;
                    format!("{}\r\n\r\n", cols[1])
                }
                "authority" => {
                    authorities += 1;
                    format!("CONNECT {} HTTP/1.1\r\n\r\n", cols[1])
                }
                other => panic!("unknown vector kind '{other}' ({line})"),
            };
            let parsed = parse_connect(head.as_bytes());
            match cols[2] {
                "accept" => assert!(
                    parsed.is_some(),
                    "'{}' must be accepted by the CONNECT grammar",
                    cols[1]
                ),
                "reject" => assert!(
                    parsed.is_none(),
                    "'{}' must be refused before any lookup or socket",
                    cols[1]
                ),
                other => panic!("unknown decision '{other}' ({line})"),
            }
        }
        // A truncated or emptied fixture must not pass by checking nothing, and
        // both row kinds have to survive: dropping every `request` row would
        // otherwise leave the method and version rules untested.
        assert!(requests >= 30, "request vectors shrank: {requests}");
        assert!(authorities >= 30, "authority vectors shrank: {authorities}");
    }

    /// The method and version rules, spelled out next to the fixture so a
    /// failure says which standard rule was broken.
    ///
    /// RFC 9112 §3.1: the method is case-sensitive. RFC 9112 §2.3: the version
    /// is `HTTP/DIGIT.DIGIT` and case-sensitive. `HTTP/1.2` is valid syntax but
    /// is refused because this proxy implements 1.0 and 1.1 only.
    #[test]
    fn only_the_connect_method_and_http_1x_are_accepted() {
        assert!(parse_connect(b"CONNECT example.test:443 HTTP/1.1\r\n\r\n").is_some());
        assert!(parse_connect(b"CONNECT example.test:443 HTTP/1.0\r\n\r\n").is_some());
        for line in [
            "connect example.test:443 HTTP/1.1",
            "Connect example.test:443 HTTP/1.1",
            "CONNECt example.test:443 HTTP/1.1",
            "CONNECT example.test:443 HTTP/1.2",
            "CONNECT example.test:443 HTTP/1.",
            "CONNECT example.test:443 HTTP/1.foo",
            "CONNECT example.test:443 HTTP/1.1x",
            "CONNECT example.test:443 http/1.1",
            "CONNECT example.test:443 HTTP/2",
        ] {
            let head = format!("{line}\r\n\r\n");
            assert!(
                parse_connect(head.as_bytes()).is_none(),
                "{line:?} must be refused"
            );
        }
    }

    /// Bytes a TSV cannot carry: a raw control byte and a non-ASCII byte in the
    /// target. Both must die in the parser, not in the resolver.
    #[test]
    fn control_and_non_ascii_bytes_never_reach_the_resolver() {
        for head in [
            "CONNECT a\u{9}b.test:443 HTTP/1.1\r\n\r\n",
            "CONNECT a\u{1}b.test:443 HTTP/1.1\r\n\r\n",
            "CONNECT \u{e9}xample.test:443 HTTP/1.1\r\n\r\n",
            // A control byte in a header line, not the target, is refused too.
            "CONNECT a.test:443 HTTP/1.1\r\nHost: a\u{1}b\r\n\r\n",
        ] {
            assert!(
                parse_connect(head.as_bytes()).is_none(),
                "{head:?} must be refused"
            );
        }
    }

    /// The alternative IPv4 spellings the WHATWG parser normalises, at the
    /// CONNECT level.
    ///
    /// `parse::<IpAddr>` rejects every one of these, so the only thing that
    /// stops the proxy from handing them to the resolver is `classify`'s
    /// `is_private_host` call. The scripted answer is PUBLIC on purpose: if that
    /// call were removed, the lookup would succeed and the request would be
    /// tunnelled, so this test fails instead of silently reopening the hole.
    /// The Kotlin suite carries the same vectors
    /// (`alternative_ipv4_spellings_and_ip_literals_are_refused`).
    #[tokio::test]
    async fn alternative_ipv4_spellings_are_refused_at_the_proxy() {
        let resolver = ScriptedResolver::new(vec![public_answer()]);
        let proxy = start_with(resolver.clone(), Policy::PUBLIC_ONLY, test_limits())
            .await
            .expect("proxy");
        for authority in [
            "0177.0.0.1:443",
            "0x7f000001:443",
            "2130706433:443",
            "127.1:443",
            "169.254.169.254:443",
        ] {
            let status = connect_via(proxy.port(), authority).await;
            assert!(status.starts_with("HTTP/1.1 403"), "{authority}: {status}");
        }
        assert_eq!(
            resolver.calls(),
            0,
            "a numeric spelling is a literal, not a name: it must not be resolved"
        );
        proxy.stop();
    }

    #[test]
    fn only_connect_with_an_authority_is_accepted() {
        assert_eq!(
            parse_connect(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n"),
            Some(("example.test".to_string(), 443))
        );
        assert!(parse_connect(b"GET http://example.test/ HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT http://example.test:443 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT user@example.test:443 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT example.test HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT example.test:0 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT example.test:443 HTTP/2.0\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT example.test:443 HTTP/1.1 extra\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT :443 HTTP/1.1\r\n\r\n").is_none());
        // Round 22: forms Android already refused and desktop tunnelled.
        assert!(parse_connect(b"CONNECT example.test:+443 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT example.test:0000443 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT [8.8.8.8]:443 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT example.test:443:443 HTTP/1.1\r\n\r\n").is_none());
        assert_eq!(
            parse_connect(b"CONNECT [::1]:443 HTTP/1.1\r\n\r\n"),
            Some(("[::1]".to_string(), 443))
        );
    }
}
