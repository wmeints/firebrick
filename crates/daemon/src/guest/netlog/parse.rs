//! The lines the logger in the guest writes: one JSON event per line, either a DNS answer or
//! a connection attempt. Lines that are too long, malformed or name an invalid host are
//! rejected without ending the connection.

use super::Protocol;
use super::record::{Recorder, Source};
use firebrick_utils::ipc;
use firebrick_utils::strip_control;
use serde::Deserialize;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};

/// The longest line fbkd reads from the logger, in bytes, without its newline.
pub(super) const MAX_LINE: usize = 4 * 1024;

/// An event the logger in the guest reports. Fields other than these, such as the guest's
/// timestamp `at`, are ignored: fbkd records when it received the event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(super) enum Event {
    /// A DNS query and the IPv4 addresses in its answer.
    Dns {
        name: String,
        addresses: Vec<Ipv4Addr>,
    },
    /// An outbound connection attempt.
    Connect {
        protocol: Protocol,
        ip: Ipv4Addr,
        port: u16,
    },
}

/// Why fbkd dropped a line from the logger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Invalid {
    /// The line is longer than [`MAX_LINE`] bytes.
    TooLong,
    /// The line isn't a JSON event of a known kind with valid fields.
    Malformed(String),
    /// The name of a DNS event isn't a valid DNS name.
    Name,
    /// The port of a connect event is 0.
    Port,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::TooLong => write!(f, "the line is longer than {MAX_LINE} bytes"),
            Invalid::Malformed(err) => write!(f, "{}", strip_control(err)),
            Invalid::Name => write!(f, "the name isn't a valid DNS name"),
            Invalid::Port => write!(f, "the port is 0"),
        }
    }
}

/// Parses and checks a line from the logger, without its newline. Returns the event with its
/// DNS name in lowercase and without a trailing dot.
pub(super) fn parse_line(line: &[u8]) -> Result<Event, Invalid> {
    if line.len() > MAX_LINE {
        return Err(Invalid::TooLong);
    }

    match serde_json::from_slice(line).map_err(|err| Invalid::Malformed(err.to_string()))? {
        Event::Dns { name, addresses } => Ok(Event::Dns {
            name: dns_name(&name).ok_or(Invalid::Name)?,
            addresses,
        }),
        Event::Connect { port: 0, .. } => Err(Invalid::Port),
        event @ Event::Connect { .. } => Ok(event),
    }
}

/// Returns the name in lowercase and without a trailing dot when it's a valid DNS name: at most
/// 253 bytes of dot-separated labels of 1 to 63 ASCII letters, digits, dashes or underscores.
///
/// This is looser than the host name check of allow rules in `firebrick-spec`, on purpose: it
/// records what the guest looked up, and real lookups include names such as `_dmarc.example.com`
/// that are valid in DNS but no host name an allow rule would name.
fn dns_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let valid = !name.is_empty() && name.len() <= 253 && name.split('.').all(is_dns_label);

    valid.then(|| name.to_ascii_lowercase())
}

/// Whether the label is 1 to 63 ASCII letters, digits, dashes or underscores.
fn is_dns_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A line read from the logger.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// A line of at most [`MAX_LINE`] bytes, now in the buffer without its newline.
    Complete,
    /// A line longer than [`MAX_LINE`] bytes, which was skipped.
    TooLong,
    /// The connection ended.
    End,
}

/// Reads the next line into `buf`, reading at most [`MAX_LINE`] bytes of it into memory.
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<Line> {
    buf.clear();
    let limit = u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX);
    let read = (&mut *reader).take(limit).read_until(b'\n', buf).await?;

    if read == 0 {
        return Ok(Line::End);
    }

    if buf.last() == Some(&b'\n') {
        buf.pop();
    }

    if buf.len() <= MAX_LINE {
        return Ok(Line::Complete);
    }

    skip_line(reader).await?;

    Ok(Line::TooLong)
}

/// Skips the rest of the current line, including its newline.
async fn skip_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf().await?;

        if available.is_empty() {
            return Ok(());
        }

        let (consumed, done) = match available.iter().position(|&b| b == b'\n') {
            Some(newline) => (newline + 1, true),
            None => (available.len(), false),
        };
        reader.consume(consumed);

        if done {
            return Ok(());
        }
    }
}

/// Records the lines of one connection from the logger until it closes.
pub(super) async fn read_events(stream: ipc::Stream, recorder: Arc<Recorder>) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::with_capacity(MAX_LINE + 1);

    loop {
        match read_line(&mut reader, &mut buf).await {
            Ok(Line::Complete) => recorder.handle_line(&buf),
            Ok(Line::TooLong) => recorder.drop_invalid(&Invalid::TooLong),
            Ok(Line::End) => return,
            Err(err) => {
                recorder.warn(Source::Guest, &format!("failed to read events: {err}"));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::netlog::tests::{dns, ip};

    #[test]
    fn parses_dns_and_connect_events() {
        let dns_line = br#"{"kind":"dns","name":"API.GitHub.com.","addresses":["140.82.112.6"],"at":"2026-10-11T14:02:11Z"}"#;
        let connect_line = br#"{"kind":"connect","protocol":"udp","ip":"140.82.112.6","port":443}"#;

        assert_eq!(
            parse_line(dns_line),
            Ok(dns("api.github.com", &["140.82.112.6"]))
        );
        assert_eq!(
            parse_line(connect_line),
            Ok(Event::Connect {
                protocol: Protocol::Udp,
                ip: ip("140.82.112.6"),
                port: 443
            })
        );
    }

    #[test]
    fn rejects_malformed_events() {
        let lines: [&[u8]; 6] = [
            b"not json",
            br#"{"kind":"http","url":"https://example.com"}"#,
            br#"{"kind":"dns","name":"example.com","addresses":["::1"]}"#,
            br#"{"kind":"connect","protocol":"tcp","ip":"300.1.1.1","port":443}"#,
            br#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":70000}"#,
            br#"{"kind":"connect","protocol":"icmp","ip":"1.1.1.1","port":1}"#,
        ];

        for line in lines {
            let result = parse_line(line);
            let text = String::from_utf8_lossy(line);
            assert!(
                matches!(result, Err(Invalid::Malformed(_))),
                "{text}: {result:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_names_ports_and_long_lines() {
        let cases: [(&[u8], Invalid); 5] = [
            (
                br#"{"kind":"dns","name":"bad name","addresses":[]}"#,
                Invalid::Name,
            ),
            (br#"{"kind":"dns","name":"","addresses":[]}"#, Invalid::Name),
            (
                br#"{"kind":"dns","name":"a..b","addresses":[]}"#,
                Invalid::Name,
            ),
            (
                br#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":0}"#,
                Invalid::Port,
            ),
            (&[b' '; MAX_LINE + 1], Invalid::TooLong),
        ];

        for (line, expected) in cases {
            assert_eq!(
                parse_line(line),
                Err(expected),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn dns_name_checks_label_and_name_lengths() {
        let label = "a".repeat(63);
        let name_253 = format!("{0}.{0}.{0}.{1}", label, "a".repeat(61));
        let name_254 = format!("{name_253}a");

        assert_eq!(dns_name(&label), Some(label.clone()));
        assert_eq!(dns_name(&"a".repeat(64)), None);
        assert_eq!(dns_name(&name_253), Some(name_253.clone()));
        assert_eq!(dns_name(&name_254), None);
        assert_eq!(
            dns_name("_dmarc.example.com"),
            Some("_dmarc.example.com".into())
        );
        assert_eq!(dns_name("."), None);
    }

    #[tokio::test]
    async fn read_line_skips_long_lines_and_keeps_reading() {
        let long = vec![b'x'; MAX_LINE + 10];
        let exact = vec![b'y'; MAX_LINE];
        let input = [&long[..], b"\n", &exact, b"\nlast"].concat();
        let mut reader = BufReader::with_capacity(16, &input[..]);
        let mut buf = vec![];

        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Line::TooLong
        );
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Line::Complete
        );
        assert_eq!(buf, exact);
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Line::Complete
        );
        assert_eq!(buf, b"last");
        assert_eq!(read_line(&mut reader, &mut buf).await.unwrap(), Line::End);
    }
}
