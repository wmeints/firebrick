//! Opens http and https URLs from sandboxes in the browser on the host.
//!
//! Each running sandbox gets one relay: a shell loop, started as an exec stream, that reads URLs
//! from the FIFO at [`FIFO_PATH`] and echoes them to stdout. The `firebrick-open` stand-in for
//! `xdg-open` in the `firebrick-base` image writes to that FIFO. fbkd reads the relay's output
//! line by line, checks each URL and hands it to an [`Opener`]. It never opens a URL whose host
//! is the host itself or its local network, or one the sandbox's enforced egress rules don't
//! allow, so the browser can't be used to get around them.
//!
//! OAuth logins in the sandbox send the browser to a `redirect_uri` on `localhost:<port>`,
//! where a callback server in the sandbox waits. Before fbkd opens such a URL, or one whose own
//! host is `localhost:<port>`, it forwards the host's `localhost:<port>` to the same port in
//! the sandbox with [`CallbackForwards`], so the browser reaches the sandbox instead of the
//! host. A URL whose own host is loopback only opens when that forward is open.

use crate::forward::{CALLBACK_IDLE_TIMEOUT, CallbackForwards, Forwards, SshConnector};
use crate::network;
use crate::rate_limit::RateLimit;
use firebrick_spec::NetworkSpec;
use firebrick_utils::strip_control;
use microsandbox::Sandbox;
use microsandbox::sandbox::exec::{ExecEvent, ExecHandle};
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::task::AbortHandle;
use url::{Host, Url};

/// Guest path of the FIFO that the stand-in writes URLs to.
pub const FIFO_PATH: &str = "/tmp/.firebrick/open.fifo";

/// The longest URL fbkd opens, in bytes.
const MAX_URL_LEN: usize = 8 * 1024;

/// The most URLs fbkd opens for one sandbox within [`OPEN_WINDOW`].
const MAX_OPENS: usize = 5;

/// The window [`MAX_OPENS`] applies to.
const OPEN_WINDOW: Duration = Duration::from_secs(10);

/// The most bytes of the relay's stderr fbkd keeps to log when the relay exits.
const MAX_STDERR: usize = 1024;

/// The relay script. It creates the FIFO's directory and the FIFO when they're missing, then
/// echoes each line written to the FIFO. It keeps the FIFO open for writing itself, so reads
/// don't end when a writer closes it.
const RELAY_SCRIPT: &str = r#"d=/tmp/.firebrick
f="$d/open.fifo"
mkdir -p -m 700 "$d" || exit 1
[ -p "$f" ] || mkfifo -m 600 "$f" || exit 1
exec 3<>"$f"
while IFS= read -r line <&3; do printf '%s\n' "$line"; done"#;

/// The name the relay script runs under (`$0`), which shows up in the guest's process list.
const RELAY_NAME: &str = "firebrick-open-relay";

/// Opens a URL on the host.
pub trait Opener: Send + Sync {
    /// Opens the URL without waiting for the program that shows it.
    fn open(&self, url: &str) -> io::Result<()>;
}

/// Opens URLs in the user's default browser with `xdg-open` on Linux and `open` on macOS.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostOpener;

impl Opener for HostOpener {
    fn open(&self, url: &str) -> io::Result<()> {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };

        // Tokio reaps the dropped child in the background, so it doesn't stay behind as a zombie.
        tokio::process::Command::new(program)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(drop)
    }
}

/// Why fbkd doesn't open a URL from a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    /// It isn't an http or https URL of at most [`MAX_URL_LEN`] bytes without whitespace or
    /// control characters.
    Invalid,
    /// Its host is the host itself or its local network, and not a loopback port that fbkd
    /// forwards to the sandbox.
    LocalHost,
    /// The sandbox's egress rules don't allow its host.
    NotAllowed,
}

/// A URL to open and the loopback ports to forward to the sandbox before opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// The URL as the browser reads it.
    url: Url,
    /// The port of the URL's own loopback host. The URL only opens when it's forwarded.
    own_port: Option<u16>,
    /// The port of the URL's loopback `redirect_uri`.
    callback_port: Option<u16>,
}

/// Checks a line from the relay and returns the URL to open with the ports to forward.
fn check_url(line: &[u8], network: &NetworkSpec) -> Result<Target, Rejection> {
    let text = std::str::from_utf8(line)
        .ok()
        .filter(|text| is_valid_url(text))
        .ok_or(Rejection::Invalid)?;
    let url = Url::parse(text).map_err(|_| Rejection::Invalid)?;
    let host = url.host().ok_or(Rejection::Invalid)?;
    let own_port = loopback_port(&url);

    // A loopback URL goes to the sandbox through the forward, so the egress rules don't apply.
    if own_port.is_none() && is_local(&host) {
        return Err(Rejection::LocalHost);
    }

    if own_port.is_none() && !network::allows_host(network, &host) {
        return Err(Rejection::NotAllowed);
    }

    Ok(Target {
        callback_port: redirect_port(&url),
        own_port,
        url,
    })
}

/// Returns the explicit, non-zero port of the URL when its host is `localhost`, `127.0.0.1` or
/// `[::1]`.
fn loopback_port(url: &Url) -> Option<u16> {
    let loopback = match url.host()? {
        Host::Domain(name) => name == "localhost",
        Host::Ipv4(ip) => ip == Ipv4Addr::LOCALHOST,
        Host::Ipv6(ip) => ip == Ipv6Addr::LOCALHOST,
    };

    url.port().filter(|&port| loopback && port != 0)
}

/// Returns the loopback port of the URL's `redirect_uri` query parameter when it's an `http`
/// URL.
fn redirect_port(url: &Url) -> Option<u16> {
    let (_, redirect) = url.query_pairs().find(|(key, _)| key == "redirect_uri")?;
    let redirect = Url::parse(&redirect)
        .ok()
        .filter(|r| r.scheme() == "http")?;

    loopback_port(&redirect)
}

/// Whether the text starts with `http://` or `https://` in any case, has no whitespace or
/// control characters and is at most [`MAX_URL_LEN`] bytes long.
fn is_valid_url(text: &str) -> bool {
    let has_scheme = ["http://", "https://"].iter().any(|scheme| {
        text.get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
    });

    has_scheme
        && text.len() <= MAX_URL_LEN
        && !text.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether the host is `localhost`, or an IP address of the host itself or its local network.
fn is_local(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        Host::Ipv4(ip) => is_local_ipv4(*ip),
        Host::Ipv6(ip) => is_local_ipv6(*ip),
    }
}

/// Whether the IPv4 address is unspecified, loopback, private, shared (CGNAT), link-local or
/// broadcast.
fn is_local_ipv4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();

    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || (first == 100 && second & 0xc0 == 64)
}

/// Whether the IPv6 address is unspecified, loopback, unique local or link-local, or an
/// IPv4-mapped local address.
fn is_local_ipv6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];

    ip.is_unspecified()
        || ip.is_loopback()
        || first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
        || ip.to_ipv4_mapped().is_some_and(is_local_ipv4)
}

/// Splits the relay's output into lines. A line longer than [`MAX_URL_LEN`] is cut off just past
/// the limit, so it fails validation without being buffered whole.
#[derive(Debug, Default)]
struct LineSplitter {
    line: Vec<u8>,
}

impl LineSplitter {
    /// Adds output and returns the lines it completes, without their `\n`.
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut parts = data.split(|&byte| byte == b'\n');
        // `split` always yields at least one part, which continues the current line.
        self.extend(parts.next().unwrap_or_default());

        parts
            .map(|part| {
                let line = std::mem::take(&mut self.line);
                self.extend(part);
                line
            })
            .collect()
    }

    /// Adds a part without `\n` to the current line, up to one byte past [`MAX_URL_LEN`].
    fn extend(&mut self, part: &[u8]) {
        let room = (MAX_URL_LEN + 1).saturating_sub(self.line.len());
        self.line.extend_from_slice(&part[..part.len().min(room)]);
    }
}

/// Handles the output of the relay of one sandbox. Dropping it closes its callback forwards.
struct RelayOutput {
    sandbox: String,
    network: NetworkSpec,
    opener: Arc<dyn Opener>,
    forwards: CallbackForwards,
    lines: LineSplitter,
    limit: RateLimit,
    stderr: Vec<u8>,
}

impl RelayOutput {
    /// Creates the handler for the relay of the sandbox with the egress rules, which forwards
    /// callback ports with `forwards`.
    fn new(
        sandbox: &str,
        network: NetworkSpec,
        opener: Arc<dyn Opener>,
        forwards: CallbackForwards,
    ) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            network,
            opener,
            forwards,
            lines: LineSplitter::default(),
            limit: RateLimit::new(MAX_OPENS, OPEN_WINDOW),
            stderr: Vec::new(),
        }
    }

    /// Opens the URLs in the relay's output, and logs when the relay fails or exits.
    async fn handle_event(&mut self, event: ExecEvent) {
        match event {
            ExecEvent::Stdout(data) => self.handle_stdout(&data).await,
            ExecEvent::Stderr(data) => self.keep_stderr(&data),
            ExecEvent::Failed(failed) => {
                let sandbox = &self.sandbox;
                tracing::warn!(error = ?failed, "failed to start the URL relay of sandbox {sandbox}");
            }
            ExecEvent::Exited { code } => self.log_exit(code),
            _ => {}
        }
    }

    /// Opens the URLs on the lines the output completes.
    async fn handle_stdout(&mut self, data: &[u8]) {
        for line in self.lines.push(data) {
            self.handle_line(&line, Instant::now()).await;
        }
    }

    /// Opens the URL on a line when it passes the checks and the rate limit, after forwarding
    /// its loopback ports, and logs a warning without the URL when it doesn't or when opening
    /// fails.
    async fn handle_line(&mut self, line: &[u8], now: Instant) {
        let sandbox = &self.sandbox;
        let target = match check_url(line, &self.network) {
            Ok(target) => target,
            Err(rejection) => return log_rejection(sandbox, rejection),
        };

        if !self.limit.allow(now) {
            tracing::warn!("ignoring a URL from sandbox {sandbox}: it opens URLs too often");
            return;
        }

        // Without its forward, a loopback URL would reach whatever listens on the host's port.
        if let Some(port) = target.own_port
            && self.forwards.ensure(port).await.is_err()
        {
            return log_rejection(sandbox, Rejection::LocalHost);
        }

        if let Some(port) = target.callback_port
            && self.forwards.ensure(port).await.is_err()
        {
            tracing::warn!(
                "opening a URL from sandbox {sandbox} without forwarding its callback port {port}; \
                 the browser sends the callback to whatever listens on localhost:{port} on the host"
            );
        }

        self.open(&target.url);
    }

    /// Opens the URL with the opener and logs the outcome without the URL.
    fn open(&self, url: &Url) {
        let sandbox = &self.sandbox;

        match self.opener.open(url.as_str()) {
            Ok(()) => tracing::info!("opened a URL from sandbox {sandbox} on the host"),
            Err(err) => tracing::warn!("failed to open a URL from sandbox {sandbox}: {err}"),
        }
    }

    /// Keeps the first [`MAX_STDERR`] bytes of the relay's stderr.
    fn keep_stderr(&mut self, data: &[u8]) {
        let room = MAX_STDERR.saturating_sub(self.stderr.len());
        self.stderr.extend_from_slice(&data[..data.len().min(room)]);
    }

    /// Logs the relay's exit, as a warning with its stderr, without control characters, when it
    /// failed.
    fn log_exit(&self, code: i32) {
        let sandbox = &self.sandbox;

        if code == 0 {
            tracing::info!("URL relay of sandbox {sandbox} exited");
        } else {
            let stderr = strip_control(&String::from_utf8_lossy(&self.stderr));
            tracing::warn!("URL relay of sandbox {sandbox} exited with {code}: {stderr}");
        }
    }
}

/// Logs why a URL from the sandbox isn't opened, without the URL.
fn log_rejection(sandbox: &str, rejection: Rejection) {
    match rejection {
        Rejection::Invalid => tracing::warn!("ignoring invalid URL from sandbox {sandbox}"),
        Rejection::LocalHost => tracing::warn!(
            "ignoring a URL from sandbox {sandbox}: it points to the host or its local network"
        ),
        Rejection::NotAllowed => tracing::warn!(
            "ignoring a URL from sandbox {sandbox}: its network rules don't allow the host"
        ),
    }
}

/// The relays of the running sandboxes, at most one per sandbox.
pub struct Relays {
    opener: Arc<dyn Opener>,
    configured: Arc<Forwards>,
    running: Arc<Mutex<HashMap<String, Running>>>,
    next_generation: AtomicU64,
}

/// The registry entry of the relay that runs for a sandbox.
struct Running {
    // So a relay that ends only removes its own entry, not that of a newer relay.
    generation: u64,
    // The relay's task, once it's spawned. Aborting it closes its callback forwards.
    task: Option<AbortHandle>,
}

impl Relays {
    /// Creates a registry whose relays open URLs with `opener`. Their callback forwards reuse
    /// the sandboxes' forwards in `configured` of the same ports.
    pub fn new(opener: Arc<dyn Opener>, configured: Arc<Forwards>) -> Self {
        Self {
            opener,
            configured,
            running: Arc::default(),
            next_generation: AtomicU64::new(0),
        }
    }

    /// Starts the relay of the connected, running sandbox unless it already has one. The relay
    /// checks URLs against the egress rules the sandbox has now. Logs a warning when the relay
    /// can't start.
    pub async fn ensure(&self, sb: &Sandbox) {
        let name = sb.name();
        let Some(generation) = self.claim(name) else {
            return;
        };

        let started = sb
            .exec_stream_with("sh", |e| e.args(["-c", RELAY_SCRIPT, RELAY_NAME]))
            .await;

        match started {
            Ok(handle) => self.spawn(sb, handle, generation),
            Err(err) => {
                tracing::warn!("failed to start the URL relay of sandbox {name}: {err}");
                release(&self.running, name, generation);
            }
        }
    }

    /// Handles the output of the sandbox's started relay in a task, with callback forwards
    /// through the sandbox's SSH server and the egress rules the sandbox has now.
    fn spawn(&self, sb: &Sandbox, handle: ExecHandle, generation: u64) {
        let name = sb.name();
        let entry = RelayEntry {
            running: self.running.clone(),
            name: name.to_string(),
            generation,
        };
        let network = network::rules_of(&sb.config().spec);
        let connector = Arc::new(SshConnector::default());
        let configured = Arc::clone(&self.configured);
        let forwards = CallbackForwards::new(name, connector, configured, CALLBACK_IDLE_TIMEOUT);
        let output = RelayOutput::new(name, network, self.opener.clone(), forwards);
        let task = tokio::spawn(run_relay(handle, entry, output));

        self.track(name, generation, task.abort_handle());
    }

    /// Forgets the relay of the sandbox, so the next [`Relays::ensure`] starts a new one, and
    /// stops it, which closes its callback forwards. Used when the sandbox stops, because its
    /// relay's exec stream may end only after that.
    pub fn forget(&self, name: &str) {
        let removed = lock(&self.running).remove(name);

        if let Some(task) = removed.and_then(|running| running.task) {
            task.abort();
        }
    }

    /// Stores the task of the relay with the generation, or stops it when the sandbox was
    /// forgotten while the relay started.
    fn track(&self, name: &str, generation: u64, task: AbortHandle) {
        let mut running = lock(&self.running);

        match running.get_mut(name).filter(|r| r.generation == generation) {
            Some(entry) => entry.task = Some(task),
            None => task.abort(),
        }
    }

    /// Registers a relay for the sandbox and returns its generation, or `None` when the sandbox
    /// already has one.
    fn claim(&self, name: &str) -> Option<u64> {
        let mut running = lock(&self.running);

        if running.contains_key(name) {
            return None;
        }

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let entry = Running {
            generation,
            task: None,
        };
        running.insert(name.to_string(), entry);

        Some(generation)
    }
}

/// The registry entry of a running relay.
struct RelayEntry {
    running: Arc<Mutex<HashMap<String, Running>>>,
    name: String,
    generation: u64,
}

/// Handles the relay's output until its exec stream ends, then removes its entry.
async fn run_relay(mut handle: ExecHandle, entry: RelayEntry, mut output: RelayOutput) {
    while let Some(event) = handle.recv().await {
        output.handle_event(event).await;
    }

    release(&entry.running, &entry.name, entry.generation);
}

/// Removes the entry of the sandbox when it still belongs to the relay with the generation.
fn release(running: &Mutex<HashMap<String, Running>>, name: &str, generation: u64) {
    let mut running = lock(running);

    if running
        .get(name)
        .is_some_and(|r| r.generation == generation)
    {
        running.remove(name);
    }
}

/// Locks the registry. Its map stays consistent when a holder panics, so a poisoned lock is
/// used as is.
fn lock(running: &Mutex<HashMap<String, Running>>) -> MutexGuard<'_, HashMap<String, Running>> {
    running.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::{Connector, GuestIo};
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Connects every forwarded connection to a port on the host's own loopback, standing in
    /// for the sandbox.
    struct LocalConnector {
        port: u16,
    }

    #[async_trait]
    impl Connector for LocalConnector {
        async fn connect(&self, _sandbox: &str, _guest_port: u16) -> io::Result<Box<dyn GuestIo>> {
            Ok(Box::new(
                TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).await?,
            ))
        }

        async fn forget(&self, _sandbox: &str) {}
    }

    /// Records the URLs it's asked to open and whether `localhost:<port>` accepted connections
    /// at that moment.
    struct ProbingOpener {
        port: u16,
        opened: Mutex<Vec<(String, bool)>>,
    }

    impl Opener for ProbingOpener {
        fn open(&self, url: &str) -> io::Result<()> {
            let listening = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).is_ok();
            self.opened
                .lock()
                .unwrap()
                .push((url.to_string(), listening));

            Ok(())
        }
    }

    impl ProbingOpener {
        fn opened(&self) -> Vec<(String, bool)> {
            self.opened.lock().unwrap().clone()
        }
    }

    /// Records the URLs it's asked to open, and fails when `fail` is set.
    #[derive(Default)]
    struct RecordingOpener {
        urls: Mutex<Vec<String>>,
        fail: bool,
    }

    impl Opener for RecordingOpener {
        fn open(&self, url: &str) -> io::Result<()> {
            self.urls.lock().unwrap().push(url.to_string());

            self.fail
                .then(|| io::Error::new(io::ErrorKind::NotFound, "no browser"))
                .map_or(Ok(()), Err)
        }
    }

    impl RecordingOpener {
        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    #[test]
    fn accepts_http_and_https_in_any_case() {
        assert!(is_valid_url("http://example.com"));
        assert!(is_valid_url("https://github.com/login/device"));
        assert!(is_valid_url("HTTPS://EXAMPLE.COM/path?q=1#x"));
        assert!(is_valid_url("hTtP://localhost:8080/callback"));
    }

    #[test]
    fn rejects_other_schemes_and_missing_scheme() {
        assert!(!is_valid_url("file:///etc/passwd"));
        assert!(!is_valid_url("javascript:alert(1)"));
        assert!(!is_valid_url("ftp://example.com"));
        assert!(!is_valid_url("example.com"));
        assert!(!is_valid_url("http:/example.com"));
        assert!(!is_valid_url(" https://example.com"));
        assert!(!is_valid_url(""));
    }

    #[test]
    fn rejects_whitespace_and_control_characters() {
        assert!(!is_valid_url("https://example.com/a b"));
        assert!(!is_valid_url("https://example.com/\t"));
        assert!(!is_valid_url("https://example.com/\r"));
        assert!(!is_valid_url("https://example.com/\u{1b}[31m"));
        assert!(!is_valid_url("https://example.com/\u{7f}"));
        assert!(!is_valid_url("https://example.com/\u{85}"));
    }

    #[test]
    fn rejects_urls_over_the_length_limit() {
        let prefix = "https://example.com/";
        let at_limit = format!("{prefix}{}", "a".repeat(MAX_URL_LEN - prefix.len()));
        let over_limit = format!("{at_limit}a");

        assert!(is_valid_url(&at_limit));
        assert!(!is_valid_url(&over_limit));
    }

    /// Returns callback forwards that connect to `localhost:<guest>` on the host.
    fn forwards_to(guest: u16) -> CallbackForwards {
        forwards_with(
            guest,
            Arc::new(Forwards::new(LocalConnector { port: guest })),
        )
    }

    /// Returns callback forwards that connect to `localhost:<guest>` on the host and reuse the
    /// `configured` forwards of sandbox `sb`.
    fn forwards_with(guest: u16, configured: Arc<Forwards>) -> CallbackForwards {
        let connector = Arc::new(LocalConnector { port: guest });

        CallbackForwards::new("sb", connector, configured, CALLBACK_IDLE_TIMEOUT)
    }

    /// Returns a handler for the relay of sandbox `sb` with the rules, and its opener. Its
    /// forwards connect to a port that nothing listens on.
    fn relay_output(network: NetworkSpec) -> (RelayOutput, Arc<RecordingOpener>) {
        let opener = Arc::new(RecordingOpener::default());
        let output = RelayOutput::new("sb", network, opener.clone(), forwards_to(1));

        (output, opener)
    }

    /// Returns a handler for the relay whose forwards reach `localhost:<guest>`, with an
    /// opener that probes `localhost:<probed>`.
    fn probing_relay_output(guest: u16, probed: u16) -> (RelayOutput, Arc<ProbingOpener>) {
        let opener = Arc::new(ProbingOpener {
            port: probed,
            opened: Mutex::default(),
        });
        let output = RelayOutput::new(
            "sb",
            NetworkSpec::default(),
            opener.clone(),
            forwards_to(guest),
        );

        (output, opener)
    }

    /// Returns a port that nothing listens on at the moment.
    async fn free_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();

        listener.local_addr().unwrap().port()
    }

    /// Starts a server on a free port that answers each connection with `pong`.
    async fn pong_server() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(answer_pong(listener));

        port
    }

    /// Answers each connection to the listener with `pong`.
    async fn answer_pong(listener: TcpListener) {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"pong").await;
        }
    }

    /// Hands the line to the handler `count` times.
    async fn handle_lines(output: &mut RelayOutput, line: &[u8], count: usize, now: Instant) {
        for _ in 0..count {
            output.handle_line(line, now).await;
        }
    }

    /// Connects to the host port and returns what the server behind it sends first.
    async fn read_from(port: u16) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.unwrap();

        String::from_utf8_lossy(&reply).into_owned()
    }

    /// Returns the ports `check_url` forwards for the line: the URL's own and its callback's.
    fn ports(line: &str) -> Result<(Option<u16>, Option<u16>), Rejection> {
        check_url(line.as_bytes(), &NetworkSpec::default())
            .map(|target| (target.own_port, target.callback_port))
    }

    fn enforced(allow: &[&str]) -> NetworkSpec {
        NetworkSpec {
            enforce: true,
            allow: allow.iter().map(|rule| rule.parse().unwrap()).collect(),
            ..NetworkSpec::default()
        }
    }

    fn check(line: &str, network: &NetworkSpec) -> Result<String, Rejection> {
        check_url(line.as_bytes(), network).map(|target| target.url.into())
    }

    #[test]
    fn check_url_returns_the_url_as_the_browser_reads_it() {
        let network = NetworkSpec::default();

        assert_eq!(
            check("HTTPS://GitHub.com/login/device", &network),
            Ok("https://github.com/login/device".into())
        );
        assert_eq!(
            check("http://example.com:8080/a?b=c#d", &network),
            Ok("http://example.com:8080/a?b=c#d".into())
        );
    }

    #[test]
    fn check_url_rejects_invalid_urls() {
        let network = NetworkSpec::default();

        for line in [
            "file:///etc/passwd",
            "https://",
            "https://exa mple.com",
            "http://[::1",
        ] {
            assert_eq!(check(line, &network), Err(Rejection::Invalid), "{line}");
        }
        assert_eq!(
            check_url(b"https://example.com/\xff", &network),
            Err(Rejection::Invalid)
        );
    }

    #[test]
    fn check_url_rejects_the_host_and_its_local_network() {
        let network = NetworkSpec::default();
        let local = [
            "http://localhost/callback",
            "http://LOCALHOST./",
            "http://app.localhost/",
            "http://127.0.0.1/",
            "http://127.1/",
            "http://2130706433/",
            "http://0x7f.0.0.1/",
            "http://0.0.0.0/",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[::]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://user@127.0.0.1/",
        ];

        for line in local {
            assert_eq!(check(line, &network), Err(Rejection::LocalHost), "{line}");
        }
        assert!(check("http://8.8.8.8/", &network).is_ok());
        assert!(check("http://[2001:db8::1]/", &network).is_ok());
        assert!(check("http://localhost.example/", &network).is_ok());
    }

    #[test]
    fn check_url_rejects_local_hosts_with_ports_it_doesnt_forward() {
        let network = NetworkSpec::default();

        for line in [
            "http://localhost:0/",
            "http://app.localhost:8080/",
            "http://10.0.0.1:8080/",
            "http://[::ffff:127.0.0.1]:8080/",
        ] {
            assert_eq!(check(line, &network), Err(Rejection::LocalHost), "{line}");
        }
    }

    #[test]
    fn check_url_forwards_the_port_of_a_loopback_host() {
        for line in [
            "http://localhost:8080/callback",
            "https://LOCALHOST:8080/",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/",
        ] {
            assert_eq!(ports(line), Ok((Some(8080), None)), "{line}");
        }
    }

    #[test]
    fn check_url_forwards_the_port_of_a_loopback_redirect_uri() {
        let redirects = [
            "http%3A%2F%2Flocalhost%3A43117%2Fcallback",
            "http://localhost:43117/callback",
            "http%3A%2F%2F127.0.0.1%3A43117%2F",
            "http%3A%2F%2F%5B%3A%3A1%5D%3A43117%2Fcb",
        ];

        for redirect in redirects {
            let line = format!("https://github.com/login?client_id=x&redirect_uri={redirect}&s=1");
            assert_eq!(ports(&line), Ok((None, Some(43117))), "{line}");
        }
    }

    #[test]
    fn check_url_forwards_both_ports_of_a_loopback_url_with_a_redirect_uri() {
        let line = "http://localhost:3000/login?redirect_uri=http%3A%2F%2Flocalhost%3A4000%2Fcb";

        assert_eq!(ports(line), Ok((Some(3000), Some(4000))));
    }

    #[test]
    fn check_url_forwards_nothing_for_other_redirect_uris() {
        let redirects = [
            "http%3A%2F%2Flocalhost%2Fcallback",
            "http%3A%2F%2Flocalhost%3A0%2F",
            "https%3A%2F%2Flocalhost%3A43117%2F",
            "http%3A%2F%2Fexample.com%3A43117%2F",
            "http%3A%2F%2F10.0.0.1%3A43117%2F",
            "not%20a%20url",
            "http%3A%2F%2Flocalhost%3A99999%2F",
            "",
        ];

        for redirect in redirects {
            let line = format!("https://github.com/login?redirect_uri={redirect}");
            assert_eq!(ports(&line), Ok((None, None)), "{line}");
        }
        assert_eq!(
            ports("https://github.com/login?redirect_url=http%3A%2F%2Flocalhost%3A43117%2F"),
            Ok((None, None))
        );
        assert_eq!(ports("https://example.com:8080/"), Ok((None, None)));
    }

    #[test]
    fn check_url_lets_loopback_urls_past_enforced_network_rules() {
        let network = enforced(&["github.com"]);

        let target = check_url(b"http://localhost:8080/", &network).unwrap();

        assert_eq!(target.url.as_str(), "http://localhost:8080/");
        assert_eq!(target.own_port, Some(8080));
        assert_eq!(
            check(
                "https://example.com/?redirect_uri=http://localhost:8080/",
                &network
            ),
            Err(Rejection::NotAllowed)
        );
    }

    #[test]
    fn check_url_applies_enforced_network_rules() {
        let network = enforced(&["github.com", "*.anthropic.com"]);

        assert!(check("https://github.com/login/device", &network).is_ok());
        assert!(check("https://console.anthropic.com/oauth", &network).is_ok());
        assert_eq!(
            check("https://attacker.example/?d=secret", &network),
            Err(Rejection::NotAllowed)
        );
        assert_eq!(
            check("https://github.com.attacker.example/", &network),
            Err(Rejection::NotAllowed)
        );
        assert_eq!(
            check("https://github.com@attacker.example/", &network),
            Err(Rejection::NotAllowed)
        );
    }

    #[tokio::test]
    async fn handle_line_opens_valid_url() {
        let (mut output, opener) = relay_output(NetworkSpec::default());

        output
            .handle_line(b"https://example.com", Instant::now())
            .await;

        assert_eq!(opener.urls(), ["https://example.com/"]);
    }

    #[tokio::test]
    async fn handle_line_ignores_rejected_lines() {
        let (mut output, opener) = relay_output(enforced(&["example.com"]));
        let now = Instant::now();

        output.handle_line(b"file:///etc/passwd", now).await;
        output.handle_line(b"https://example.com/\xff", now).await;
        output.handle_line(b"", now).await;
        output.handle_line(b"http://10.0.0.1:3000/", now).await;
        output.handle_line(b"https://example.org/", now).await;

        assert!(opener.urls().is_empty());
    }

    #[tokio::test]
    async fn handle_line_stops_opening_urls_over_the_rate_limit() {
        let (mut output, opener) = relay_output(NetworkSpec::default());
        let now = Instant::now();

        handle_lines(&mut output, b"https://example.com/", MAX_OPENS + 1, now).await;

        assert_eq!(opener.urls().len(), MAX_OPENS);
    }

    #[tokio::test]
    async fn handle_line_survives_failing_opener() {
        let opener = Arc::new(RecordingOpener {
            fail: true,
            ..Default::default()
        });
        let mut output =
            RelayOutput::new("sb", NetworkSpec::default(), opener.clone(), forwards_to(1));

        output
            .handle_line(b"https://example.com/", Instant::now())
            .await;
        output
            .handle_line(b"https://example.org/", Instant::now())
            .await;

        assert_eq!(
            opener.urls(),
            ["https://example.com/", "https://example.org/"]
        );
    }

    #[tokio::test]
    async fn handle_event_opens_urls_split_over_chunks() {
        let (mut output, opener) = relay_output(NetworkSpec::default());

        output
            .handle_event(ExecEvent::Stdout("https://exa".into()))
            .await;
        output
            .handle_event(ExecEvent::Stdout(
                "mple.com/\nhttps://example.org/\n".into(),
            ))
            .await;

        assert_eq!(
            opener.urls(),
            ["https://example.com/", "https://example.org/"]
        );
    }

    #[tokio::test]
    async fn handle_event_keeps_the_start_of_stderr() {
        let (mut output, _) = relay_output(NetworkSpec::default());

        output
            .handle_event(ExecEvent::Stderr("mkfifo: not found\n".into()))
            .await;
        output
            .handle_event(ExecEvent::Stderr(vec![b'x'; 2 * MAX_STDERR].into()))
            .await;

        assert!(output.stderr.starts_with(b"mkfifo: not found\n"));
        assert_eq!(output.stderr.len(), MAX_STDERR);
    }

    #[tokio::test]
    async fn handle_line_forwards_the_redirect_port_before_opening() {
        let (guest, port) = (pong_server().await, free_port().await);
        let (mut output, opener) = probing_relay_output(guest, port);
        let url =
            format!("https://github.com/login?redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcb");

        output.handle_line(url.as_bytes(), Instant::now()).await;

        assert_eq!(opener.opened(), [(url, true)]);
        assert_eq!(read_from(port).await, "pong");
    }

    #[tokio::test]
    async fn handle_line_forwards_the_port_of_a_loopback_url_before_opening() {
        let (guest, port) = (pong_server().await, free_port().await);
        let (mut output, opener) = probing_relay_output(guest, port);
        let url = format!("http://localhost:{port}/");

        output.handle_line(url.as_bytes(), Instant::now()).await;

        assert_eq!(opener.opened(), [(url, true)]);
        assert_eq!(read_from(port).await, "pong");
    }

    #[tokio::test]
    async fn handle_line_reuses_the_forward_of_a_port() {
        let (guest, port) = (pong_server().await, free_port().await);
        let (mut output, opener) = probing_relay_output(guest, port);
        let url = format!("http://127.0.0.1:{port}/");

        output.handle_line(url.as_bytes(), Instant::now()).await;
        output.handle_line(url.as_bytes(), Instant::now()).await;

        assert_eq!(opener.opened(), [(url.clone(), true), (url, true)]);
        assert_eq!(read_from(port).await, "pong");
    }

    #[tokio::test]
    async fn handle_line_opens_a_url_whose_redirect_port_is_busy() {
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = busy.local_addr().unwrap().port();
        let (mut output, opener) = relay_output(NetworkSpec::default());
        let url = format!("https://github.com/login?redirect_uri=http://localhost:{port}/cb");

        output.handle_line(url.as_bytes(), Instant::now()).await;

        assert_eq!(opener.urls(), [url]);
    }

    #[tokio::test]
    async fn handle_line_opens_a_loopback_url_on_a_configured_forward_of_the_port() {
        let (guest, port) = (pong_server().await, free_port().await);
        let configured = Arc::new(Forwards::new(LocalConnector { port: guest }));
        configured
            .apply(
                "sb",
                &[firebrick_spec::PortMapping {
                    host: port,
                    guest: port,
                }],
            )
            .await;
        let opener = Arc::new(RecordingOpener::default());
        let forwards = forwards_with(guest, configured);
        let mut output = RelayOutput::new("sb", NetworkSpec::default(), opener.clone(), forwards);
        let url = format!("http://localhost:{port}/");

        output.handle_line(url.as_bytes(), Instant::now()).await;

        assert_eq!(opener.urls(), [url]);
        assert_eq!(read_from(port).await, "pong");
    }

    #[tokio::test]
    async fn handle_line_ignores_a_loopback_url_whose_port_is_busy() {
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = busy.local_addr().unwrap().port();
        let (mut output, opener) = relay_output(NetworkSpec::default());

        output
            .handle_line(
                format!("http://localhost:{port}/").as_bytes(),
                Instant::now(),
            )
            .await;

        assert!(opener.urls().is_empty());
    }

    #[test]
    fn line_splitter_joins_chunks_and_splits_lines() {
        let mut lines = LineSplitter::default();

        assert!(lines.push(b"https://exa").is_empty());
        assert_eq!(
            lines.push(b"mple.com\nhttps://b.example\nhttps://c"),
            [
                b"https://example.com".to_vec(),
                b"https://b.example".to_vec()
            ]
        );
        assert_eq!(lines.push(b".example\n"), [b"https://c.example".to_vec()]);
    }

    #[test]
    fn line_splitter_cuts_long_lines_so_they_fail_validation() {
        let mut lines = LineSplitter::default();
        let long = format!("https://example.com/{}\n", "a".repeat(2 * MAX_URL_LEN));

        let split = lines.push(long.as_bytes());

        assert_eq!(split.len(), 1);
        assert_eq!(split[0].len(), MAX_URL_LEN + 1);
        assert_eq!(
            check_url(&split[0], &NetworkSpec::default()),
            Err(Rejection::Invalid)
        );
    }

    #[test]
    fn forget_lets_a_new_relay_claim_the_sandbox() {
        let configured = Arc::new(Forwards::new(LocalConnector { port: 1 }));
        let relays = Relays::new(Arc::new(HostOpener), configured);

        let first = relays.claim("sb").unwrap();
        assert_eq!(relays.claim("sb"), None);

        relays.forget("sb");
        let second = relays.claim("sb").unwrap();

        // The first relay ending doesn't remove the entry of the second.
        release(&relays.running, "sb", first);
        assert_eq!(relays.claim("sb"), None);
        release(&relays.running, "sb", second);
        assert!(relays.claim("sb").is_some());
    }
}
