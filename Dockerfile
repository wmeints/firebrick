# syntax=docker/dockerfile:1
FROM ubuntu:26.04

ARG DEBIAN_FRONTEND=noninteractive

# Base tooling, a native build toolchain, ICU for .NET globalization (such as the Aspire CLI),
# sudo, procps (sysctl), tini, python3 and tcpdump for the network logger, the mise apt
# repository and the Docker engine from Docker's apt repository.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        gpg \
        libicu78 \
        libssl-dev \
        pkg-config \
        procps \
        python3 \
        sudo \
        tcpdump \
        tini \
    && install -dm 755 /etc/apt/keyrings \
    && curl -fsSL https://mise.jdx.dev/gpg-key.pub | gpg --dearmor -o /etc/apt/keyrings/mise-archive-keyring.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/mise-archive-keyring.gpg arch=$(dpkg --print-architecture)] https://mise.jdx.dev/deb stable main" \
        > /etc/apt/sources.list.d/mise.list \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        containerd.io \
        docker-buildx-plugin \
        docker-ce \
        docker-ce-cli \
        docker-compose-plugin \
        mise \
    && rm -rf /var/lib/apt/lists/*

# Microsandbox gives the guest IPv6 even when the host can't route it, and then resets IPv6
# connections after the handshake, so clients never fall back to IPv4. Disable guest IPv6
# until microsandbox handles this: https://github.com/superradcompany/microsandbox/issues/1226
COPY <<EOF /etc/sysctl.d/99-disable-ipv6.conf
net.ipv6.conf.all.disable_ipv6=1
net.ipv6.conf.default.disable_ipv6=1
EOF

# firebrick-netlog reports the guest's DNS lookups and outbound connection attempts to fbkd,
# which records them per sandbox (ADR 0038). Two tcpdumps on the default route's interface
# capture outgoing UDP datagrams and TCP SYNs, also the ones the network policy denies, and the
# answers of DNS servers. The script sends one JSON event per line to vsock port 52000 on the
# host (CID 2). It never blocks the capture: while fbkd can't be reached, or its 64 KiB buffer
# is full, it drops events, and it reconnects with a backoff of at most 5 seconds. It runs as
# root inside the VM, so the agent can stop it or send false events; the record is a
# diagnostic aid, not an audit trail.
COPY --chmod=755 <<'EOF' /usr/local/sbin/firebrick-netlog
#!/usr/bin/python3
import datetime
import json
import re
import selectors
import socket
import subprocess
import sys
import time

HOST_CID = 2
PORT = 52000
MAX_BUFFER = 64 * 1024
MAX_BACKOFF = 5.0
MAX_TRACKED = 4096
OUTGOING = "udp or (tcp[tcpflags] & (tcp-syn|tcp-ack) == tcp-syn)"
DNS_ANSWERS = "udp src port 53"

PACKET = re.compile(r" IP (\d+\.\d+\.\d+\.\d+)\.(\d+) > (\d+\.\d+\.\d+\.\d+)\.(\d+): (.*)")
QUERY = re.compile(r"^(\d+)\S*(?: \[\S+\])* A\? (\S+) ")
ANSWER_ID = re.compile(r"^(\d+)\D")
A_RECORD = re.compile(r"\bA (\d+\.\d+\.\d+\.\d+)")


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def default_interface():
    with open("/proc/net/route") as routes:
        for line in routes.readlines()[1:]:
            fields = line.split()
            if len(fields) > 1 and fields[1] == "00000000":
                return fields[0]
    return "eth0"


class Sender:
    """Sends events to fbkd without ever blocking, dropping them while it can't."""

    def __init__(self):
        self.sock = None
        self.buffer = bytearray()
        self.backoff = 0.5
        self.retry_at = 0.0

    def send(self, event):
        event["at"] = now()
        line = json.dumps(event, separators=(",", ":")).encode() + b"\n"
        self.check()
        if self.sock is None:
            self.connect()
        if self.sock is not None and len(self.buffer) + len(line) <= MAX_BUFFER:
            self.buffer += line
        self.flush()

    def connect(self):
        if time.monotonic() < self.retry_at:
            return
        sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        try:
            sock.settimeout(1)
            sock.connect((HOST_CID, PORT))
            sock.setblocking(False)
        except OSError:
            sock.close()
            self.retry_at = time.monotonic() + self.backoff
            self.backoff = min(self.backoff * 2, MAX_BACKOFF)
            return
        self.sock = sock
        self.backoff = 0.5

    def check(self):
        """Notices that fbkd closed the connection, for example because it restarted."""
        if self.sock is None:
            return
        try:
            closed = self.sock.recv(1, socket.MSG_PEEK) == b""
        except BlockingIOError:
            closed = False
        except OSError:
            closed = True
        if closed:
            self.disconnect()

    def disconnect(self):
        self.sock.close()
        self.sock = None
        self.buffer.clear()

    def flush(self):
        if self.sock is None or not self.buffer:
            return
        try:
            sent = self.sock.send(self.buffer)
            del self.buffer[:sent]
        except BlockingIOError:
            pass
        except OSError:
            self.disconnect()


class Parser:
    """Turns tcpdump's lines into events."""

    def __init__(self, sender):
        self.sender = sender
        self.queries = {}
        self.udp_seen = set()

    def outgoing(self, line):
        match = PACKET.search(line)
        if not match:
            return
        _, src_port, ip, port, rest = match.groups()
        if port == "53":
            self.query(src_port, rest)
        if "Flags [S" in rest:
            self.sender.send({"kind": "connect", "protocol": "tcp", "ip": ip, "port": int(port)})
        elif (ip, port) not in self.udp_seen:
            if len(self.udp_seen) >= MAX_TRACKED:
                self.udp_seen.clear()
            self.udp_seen.add((ip, port))
            self.sender.send({"kind": "connect", "protocol": "udp", "ip": ip, "port": int(port)})

    def query(self, src_port, rest):
        match = QUERY.match(rest)
        if not match:
            return
        if len(self.queries) >= MAX_TRACKED:
            self.queries.pop(next(iter(self.queries)))
        self.queries[(src_port, match.group(1))] = match.group(2).rstrip(".")

    def answer(self, line):
        match = PACKET.search(line)
        if not match:
            return
        dst_port, rest = match.group(4), match.group(5)
        answer_id = ANSWER_ID.match(rest)
        name = answer_id and self.queries.pop((dst_port, answer_id.group(1)), None)
        if name:
            addresses = A_RECORD.findall(rest)
            self.sender.send({"kind": "dns", "name": name, "addresses": addresses})


def tcpdump(interface, direction, expression):
    command = ["tcpdump", "-i", interface, "-n", "-l", "-p", "--immediate-mode", "-Q", direction]
    return subprocess.Popen(command + [expression], stdout=subprocess.PIPE, text=True, bufsize=1)


def main():
    interface = default_interface()
    sender = Sender()
    parser = Parser(sender)
    captures = [
        (tcpdump(interface, "out", OUTGOING), parser.outgoing),
        (tcpdump(interface, "in", DNS_ANSWERS), parser.answer),
    ]
    selector = selectors.DefaultSelector()
    for process, handle in captures:
        selector.register(process.stdout, selectors.EVENT_READ, handle)
    while True:
        for key, _ in selector.select(timeout=1):
            line = key.fileobj.readline()
            if not line:
                for process, _ in captures:
                    process.kill()
                sys.exit("firebrick-netlog: tcpdump exited")
            key.data(line)
        sender.check()
        sender.flush()


main()
EOF

# fbkd hands PID 1 to /sbin/init in sandboxes that run this image. The script applies the
# sysctl settings, remounts devpts, starts dockerd and the network logger in the background without waiting for
# them, and hands PID 1 to tini, which reaps zombie processes. The logger restarts when it
# exits and logs to /var/log/firebrick-netlog.log. dockerd keeps its data on the ext4 disk that fbkd
# attaches at /var/lib/docker, and logs to /var/log/dockerd.log. /run lives on the sandbox's
# persistent root instead of a tmpfs, so the init removes the runtime state of the previous
# boot first; otherwise a stale docker.pid stops dockerd from starting after fbk stop/start.
# microsandbox's agent opens session terminals as root and doesn't hand them to the session
# user, so the agent can't open its own /dev/pts/N by path; `stty -F`, which Quarkus' and other
# interactive consoles use, fails and Ctrl+C leaves their child processes running. Remounting
# devpts gives new terminals to the agent group (ADR 0039).
COPY --chmod=755 <<EOF /sbin/init
#!/bin/sh
sysctl -q -p /etc/sysctl.d/99-disable-ipv6.conf
mount -o remount,gid=1000,mode=0660 /dev/pts
rm -rf /run/docker.pid /run/docker /run/containerd
/usr/bin/dockerd >/var/log/dockerd.log 2>&1 &
(while :; do /usr/local/sbin/firebrick-netlog; sleep 5; done) >>/var/log/firebrick-netlog.log 2>&1 &
exec /usr/bin/tini -- sleep infinity
EOF

# SSH clients send their own TERM, which the guest may have no terminfo entry for, such as
# `xterm-ghostty`. Interactive shells fall back to xterm-256color for those.
RUN echo 'infocmp "$TERM" >/dev/null 2>&1 || export TERM=xterm-256color' >> /etc/bash.bashrc

# The guest has no browser, so firebrick-open stands in for xdg-open and $BROWSER: it hands
# http(s) URLs to fbkd through the FIFO that fbkd's URL relay reads, and fbkd opens them in the
# browser on the host. Without a relay it gives up after 5 seconds and prints the URL instead.
COPY --chmod=755 <<'EOF' /usr/local/bin/firebrick-open
#!/bin/sh
fifo=/tmp/.firebrick/open.fifo

invalid() {
    printf '%s\n' "firebrick-open: only http and https URLs can be opened on the host" >&2
    exit 2
}

[ "$#" -eq 1 ] || invalid
url=$1

case "$url" in
    [Hh][Tt][Tt][Pp]://* | [Hh][Tt][Tt][Pp][Ss]://*) ;;
    *) invalid ;;
esac

case "$url" in
    *[[:space:][:cntrl:]]*) invalid ;;
esac

# Writing to a missing FIFO would create a regular file, so check that the relay made it.
if [ ! -p "$fifo" ] || ! timeout 5 sh -c 'printf "%s\n" "$1" > "$2"' _ "$url" "$fifo" 2>/dev/null; then
    printf "firebrick-open: couldn't reach the host; open this URL yourself: %s\n" "$url" >&2
    exit 1
fi
EOF
RUN ln -s /usr/local/bin/firebrick-open /usr/local/bin/xdg-open
ENV BROWSER=/usr/local/bin/xdg-open

# The guest has no display server either, so firebrick-paste stands in for wl-paste and xclip:
# it reads the host clipboard's text and PNG content through fbkd on vsock port 52001, which
# sandboxes created with `clipboard: true` in .firebrick.yml have a route to (ADR 0048). It
# behaves like the tool it's called as and supports only pasting from the clipboard selection.
COPY --chmod=755 image/firebrick-paste /usr/local/bin/firebrick-paste
RUN ln -s /usr/local/bin/firebrick-paste /usr/local/bin/wl-paste \
    && ln -s /usr/local/bin/firebrick-paste /usr/local/bin/xclip

# git over HTTPS to github.com authenticates with GH_TOKEN, or GITHUB_TOKEN when GH_TOKEN is
# unset or empty. In a sandbox the variable holds a microsandbox placeholder, which the TLS
# proxy replaces in the Basic Authorization header for the secret's allowed hosts. Without a
# token the helper answers nothing, so git tries the next helper or prompts. It's registered
# in /etc/gitconfig, so helpers in the agent's own ~/.gitconfig keep working.
COPY --chmod=755 <<'EOF' /usr/local/bin/firebrick-git-credential
#!/bin/sh
cat >/dev/null
[ "$1" = get ] || exit 0
token=${GH_TOKEN:-$GITHUB_TOKEN}
[ -n "$token" ] || exit 0
printf 'username=x-access-token\npassword=%s\n' "$token"
EOF
RUN git config --system credential.https://github.com.helper /usr/local/bin/firebrick-git-credential

# Replace the default ubuntu user (uid/gid 1000) with the agent user, who can use docker
# without sudo through the docker group.
RUN userdel --remove ubuntu \
    && groupadd --gid 1000 agent \
    && useradd --uid 1000 --gid 1000 --groups docker --create-home --shell /bin/bash agent \
    && echo "agent ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/agent \
    && chmod 0440 /etc/sudoers.d/agent

USER agent
WORKDIR /home/agent

# Activate mise for interactive shells and expose its shims to everything else.
RUN echo 'eval "$(mise activate bash)"' >> /home/agent/.bashrc
ENV PATH="/home/agent/.local/share/mise/shims:${PATH}"

CMD ["/bin/bash"]
