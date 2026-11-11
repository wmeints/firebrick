//! Parses the output of the listener script: the guest's `/proc/net/tcp` and `/proc/net/tcp6`,
//! and the socket descriptors and names of its processes. The guest writes all of it, so lines
//! that don't parse are skipped and the result is bounded.

use super::{Listener, Process, Reading};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The line that starts the `/proc/net/tcp` section.
pub(super) const TCP_MARKER: &str = "==tcp==";
/// The line that starts the `/proc/net/tcp6` section.
pub(super) const TCP6_MARKER: &str = "==tcp6==";
/// The line that starts the processes section.
pub(super) const PROCS_MARKER: &str = "==procs==";

/// The most listeners returned, so a guest that opens many sockets can't flood the client.
pub(super) const MAX_LISTENERS: usize = 1024;
/// The most characters of a process name that are kept.
pub(super) const MAX_PROCESS_NAME: usize = 64;

/// The `st` value of a socket in the LISTEN state.
const LISTEN_STATE: &str = "0A";

/// The section of the output a line belongs to.
#[derive(Clone, Copy)]
enum Section {
    /// Before the first marker.
    None,
    /// `/proc/net/tcp` or `/proc/net/tcp6`.
    Sockets(Family),
    /// `pid <pid> <name>` lines, each followed by the `ls -l` lines of its `fd` directory.
    Procs,
}

/// The address family of a socket table.
#[derive(Clone, Copy)]
enum Family {
    V4,
    V6,
}

/// A listening socket from a socket table.
struct Socket {
    address: IpAddr,
    port: u16,
    inode: u64,
}

/// The listening sockets and the processes that own them, collected line by line.
#[derive(Default)]
struct Collected {
    sockets: Vec<Socket>,
    /// The lowest pid that holds each socket inode, with its name.
    owners: HashMap<u64, Process>,
    /// The process whose `fd` lines follow.
    current: Option<Process>,
}

/// Returns the listening TCP ports in the output, sorted by port, at most [`MAX_LISTENERS`]. The
/// reading is truncated when the output has more.
pub(super) fn listeners(output: &str) -> Reading {
    let mut section = Section::None;
    let mut collected = Collected::default();

    for line in output.lines() {
        match line {
            TCP_MARKER => section = Section::Sockets(Family::V4),
            TCP6_MARKER => section = Section::Sockets(Family::V6),
            PROCS_MARKER => section = Section::Procs,
            _ => collected.add_line(section, line),
        }
    }

    collected.into_listeners()
}

impl Collected {
    fn add_line(&mut self, section: Section, line: &str) {
        match section {
            Section::None => {}
            Section::Sockets(family) => self.sockets.extend(socket(family, line)),
            Section::Procs => self.add_proc_line(line),
        }
    }

    /// Starts a new process on a `pid` line, or records the socket of an `fd` line as owned by
    /// the current process.
    fn add_proc_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("pid ") {
            self.current = process(rest);
            return;
        }

        let (Some(owner), Some(inode)) = (&self.current, socket_inode(line)) else {
            return;
        };

        let known = self.owners.entry(inode).or_insert_with(|| owner.clone());
        if owner.pid < known.pid {
            *known = owner.clone();
        }
    }

    /// Groups the sockets by port, with the lowest pid that owns one of them.
    fn into_listeners(self) -> Reading {
        let mut ports: BTreeMap<u16, (BTreeSet<IpAddr>, Option<&Process>)> = BTreeMap::new();

        for socket in &self.sockets {
            let (addresses, process) = ports.entry(socket.port).or_default();
            addresses.insert(socket.address);
            let owner = self.owners.get(&socket.inode);
            *process = (*process).into_iter().chain(owner).min_by_key(|p| p.pid);
        }

        let truncated = ports.len() > MAX_LISTENERS;
        let listeners = ports
            .into_iter()
            .take(MAX_LISTENERS)
            .map(|(port, (addresses, process))| Listener {
                port,
                addresses: addresses.into_iter().collect(),
                process: process.cloned(),
            })
            .collect();

        Reading {
            listeners,
            truncated,
        }
    }
}

/// Parses a row of a socket table, such as
/// `0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 12345 ...`,
/// and returns it when the socket listens on a nonzero port.
fn socket(family: Family, line: &str) -> Option<Socket> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let (local, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);

    if *state != LISTEN_STATE {
        return None;
    }

    let (address, port) = local.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok().filter(|&p| p != 0)?;

    Some(Socket {
        address: address_of(family, address)?,
        port,
        inode: inode.parse().ok()?,
    })
}

/// Decodes the hex address of a socket table. The kernel prints each 32-bit word of the address
/// in the guest's byte order, which is little-endian on the architectures microsandbox runs.
fn address_of(family: Family, hex: &str) -> Option<IpAddr> {
    let words = hex_words(hex)?;

    match family {
        Family::V4 => <[u32; 1]>::try_from(words)
            .ok()
            .map(|[word]| Ipv4Addr::from(word.to_le_bytes()).into()),
        Family::V6 => {
            let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
            <[u8; 16]>::try_from(bytes)
                .ok()
                .map(|bytes| Ipv6Addr::from(bytes).into())
        }
    }
}

/// Splits the hex digits into 32-bit words of 8 digits each.
fn hex_words(hex: &str) -> Option<Vec<u32>> {
    if hex.is_empty() || !hex.len().is_multiple_of(8) || !hex.is_ascii() {
        return None;
    }

    (0..hex.len())
        .step_by(8)
        .map(|start| u32::from_str_radix(hex.get(start..start + 8)?, 16).ok())
        .collect()
}

/// Parses `<pid> <name>`, the rest of a `pid` line, keeping at most [`MAX_PROCESS_NAME`]
/// characters of the name.
fn process(rest: &str) -> Option<Process> {
    let (pid, name) = rest.split_once(' ').unwrap_or((rest, ""));

    Some(Process {
        pid: pid.parse().ok()?,
        name: name.chars().take(MAX_PROCESS_NAME).collect(),
    })
}

/// Returns the inode of an `ls -l` line of a socket descriptor, which ends in
/// `-> socket:[<inode>]`.
fn socket_inode(line: &str) -> Option<u64> {
    let (_, target) = line.rsplit_once(" -> socket:[")?;

    target.strip_suffix(']')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header of a socket table, which doesn't parse as a row.
    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when \
                          retrnsmt   uid  timeout inode";

    /// Returns a socket table row for the local address, state and inode.
    fn row(local: &str, state: &str, inode: u64) -> String {
        format!(
            "   0: {local} 00000000:0000 {state} 00000000:00000000 00:00000000 00000000  1000 \
             0 {inode} 1 0000000000000000 100 0 0 10 0"
        )
    }

    /// Returns a `ls -l` line of a descriptor that points at the socket.
    fn fd_line(fd: u32, inode: u64) -> String {
        format!("lrwx------ 1 root root 64 Nov 11 10:00 {fd} -> socket:[{inode}]")
    }

    fn listener(port: u16, addresses: &[&str], process: Option<(u32, &str)>) -> Listener {
        Listener {
            port,
            addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
            process: process.map(|(pid, name)| Process {
                pid,
                name: name.to_string(),
            }),
        }
    }

    #[test]
    fn decodes_ipv4_and_ipv6_listeners_and_joins_their_owners() {
        let output = [
            TCP_MARKER.to_string(),
            HEADER.to_string(),
            row("0100007F:0BB8", "0A", 100),
            row("00000000:1435", "0A", 200),
            TCP6_MARKER.to_string(),
            HEADER.to_string(),
            row("00000000000000000000000000000000:1435", "0A", 201),
            row("00000000000000000000000001000000:22B8", "0A", 300),
            PROCS_MARKER.to_string(),
            "pid 412 node".to_string(),
            "total 0".to_string(),
            "lr-x------ 1 root root 64 Nov 11 10:00 0 -> /dev/null".to_string(),
            fd_line(3, 100),
            "pid 530 vite".to_string(),
            fd_line(20, 200),
            fd_line(21, 201),
        ]
        .join("\n");

        assert_eq!(
            listeners(&output).listeners,
            [
                listener(3000, &["127.0.0.1"], Some((412, "node"))),
                listener(5173, &["0.0.0.0", "::"], Some((530, "vite"))),
                listener(8888, &["::1"], None),
            ]
        );
    }

    #[test]
    fn skips_sockets_that_do_not_listen() {
        let output = [
            TCP_MARKER.to_string(),
            row("0100007F:0BB8", "01", 100),
            row("0100007F:0BB9", "06", 101),
            row("0100007F:0000", "0A", 102),
        ]
        .join("\n");

        assert_eq!(listeners(&output).listeners, []);
    }

    #[test]
    fn names_the_lowest_pid_that_holds_a_socket() {
        let output = [
            TCP_MARKER.to_string(),
            row("00000000:0050", "0A", 100),
            PROCS_MARKER.to_string(),
            "pid 90 worker".to_string(),
            fd_line(3, 100),
            "pid 12 master".to_string(),
            fd_line(3, 100),
            "pid 40 other".to_string(),
            fd_line(3, 100),
        ]
        .join("\n");

        assert_eq!(
            listeners(&output).listeners,
            [listener(80, &["0.0.0.0"], Some((12, "master")))]
        );
    }

    #[test]
    fn skips_malformed_lines_and_keeps_the_valid_ones() {
        let output = [
            "garbage before the first marker".to_string(),
            row("0200007F:1F90", "0A", 1),
            TCP_MARKER.to_string(),
            "".to_string(),
            "   0: 0100007F:0BBB 00000000:0000 0A".to_string(),
            row("zz00007F:0BBC", "0A", 6),
            "   1: 0100007F:0BB8 00000000:0000 0A 0 0 0 0 0 not-an-inode".to_string(),
            row("0100007F:XYZ", "0A", 2),
            row("0100007F:0BB8", "0A", 3),
            TCP6_MARKER.to_string(),
            row("0100007F:0BB9", "0A", 4),
            row("é0000000000000000000000000000000:0BBA", "0A", 5),
            PROCS_MARKER.to_string(),
            "pid".to_string(),
            "pid abc name".to_string(),
            fd_line(1, 3),
            "pid 7 sh".to_string(),
            "lrwx------ 1 root root 64 Nov 11 10:00 3 -> socket:[3".to_string(),
            "lrwx------ 1 root root 64 Nov 11 10:00 3 -> socket:[x]".to_string(),
        ]
        .join("\n");

        let reading = listeners(&output);

        assert_eq!(reading.listeners, [listener(3000, &["127.0.0.1"], None)]);
        assert!(!reading.truncated);
    }

    #[test]
    fn bounds_the_listeners_and_process_names() {
        let long_name = "n".repeat(MAX_PROCESS_NAME + 10);
        let mut lines = vec![TCP_MARKER.to_string()];
        lines.extend(
            (1..=u16::try_from(MAX_LISTENERS + 5).unwrap())
                .map(|port| row(&format!("0100007F:{port:04X}"), "0A", port.into())),
        );
        lines.push(PROCS_MARKER.to_string());
        lines.push(format!("pid 5 {long_name}"));
        lines.push(fd_line(3, 1));

        let reading = listeners(&lines.join("\n"));
        let found = reading.listeners;

        assert!(reading.truncated);
        assert_eq!(found.len(), MAX_LISTENERS);
        assert_eq!(found.last().unwrap().port, 1024);
        assert_eq!(
            found[0].process.as_ref().unwrap().name,
            "n".repeat(MAX_PROCESS_NAME)
        );
    }
}
