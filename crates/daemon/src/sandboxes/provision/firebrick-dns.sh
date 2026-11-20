#!/bin/sh
# Points the guest's DNS at the host's servers (ADR 0066). fbkd runs this script as root with
# the host's DNS servers as arguments. It writes them into /etc/resolv.conf, followed by the
# gateway of the default route, which stays the fallback and the only server when there are
# no arguments. It leaves the file alone when its content doesn't change.
set -e

# The gateway of the default route. /proc/net/route holds it as the hex digits of a
# little-endian IPv4 address, such as 0100600A for 10.96.0.1.
route=
while read -r _ destination gateway _; do
    if [ "$destination" = 00000000 ] && [ "$gateway" != 00000000 ]; then
        route=$gateway
        break
    fi
done < /proc/net/route
[ -n "$route" ] || { echo "the guest has no default route" >&2; exit 1; }

b1=${route#??????}
b2=${route#????}; b2=${b2%??}
b3=${route#??}; b3=${b3%????}
b4=${route%??????}
gateway=$(printf '%d.%d.%d.%d' "0x$b1" "0x$b2" "0x$b3" "0x$b4")

content="# Written by firebrick: the host's DNS servers, then the sandbox gateway."
for server in "$@"; do
    content="$content
nameserver $server"
done
content="$content
nameserver $gateway"

[ "$(cat /etc/resolv.conf 2>/dev/null)" = "$content" ] && exit 0

# Write a new file and move it into place, so the file is never half written. Copy it instead
# when /etc/resolv.conf is a mount point that can't be replaced.
new=/etc/resolv.conf.firebrick
printf '%s\n' "$content" > "$new"
mv -f "$new" /etc/resolv.conf 2>/dev/null || { cat "$new" > /etc/resolv.conf; rm -f "$new"; }
