#!/usr/bin/env bash
# Which network setup steps a transparent proxy sidecar needs does the current
# Docker runtime support? Prints one JSON line per step. Also lists the shell
# commands proxy-everything's binary contains, to see which ones it runs.
set -uo pipefail
script='
apk add -q iproute2 iptables iptables-legacy > /dev/null 2>&1 || { echo "apk failed"; exit 0; }
try() { out=$(sh -c "$2" 2>&1); echo "$1|$?|$(echo "$out" | head -1)"; }
try addr_add_loopback_alias "ip addr add 127.0.0.3/8 dev lo"
try rule_fwmark "ip rule add fwmark 1 lookup 100"
try route_local_table "ip route add local 0.0.0.0/0 dev lo table 100"
try iptables_nat_redirect_out "iptables -t nat -A OUTPUT -p tcp ! -d 127.0.0.0/8 -j REDIRECT --to-ports 15001"
try iptables_nat_redirect_dns "iptables -t nat -A OUTPUT -p udp --dport 53 -j REDIRECT --to-ports 15053"
try iptables_mangle_tproxy "iptables -t mangle -A PREROUTING -p tcp -j TPROXY --on-port 15001 --tproxy-mark 1"
try iptables_owner_match "iptables -t nat -A OUTPUT -m owner --uid-owner 1337 -j RETURN"
try legacy_available "iptables-legacy -V"
try legacy_nat_redirect_out "iptables-legacy -t nat -A OUTPUT -p tcp ! -d 127.0.0.0/8 -j REDIRECT --to-ports 15001"
try legacy_nat_redirect_dns "iptables-legacy -t nat -A OUTPUT -p udp --dport 53 -j REDIRECT --to-ports 15053"
try legacy_owner_match "iptables-legacy -t nat -I OUTPUT -m owner --uid-owner 1337 -j RETURN"
try legacy_mangle_tproxy "iptables-legacy -t mangle -A PREROUTING -p tcp -j TPROXY --on-port 15001 --tproxy-mark 1"
try legacy_redirect_works "iptables-legacy -t nat -F OUTPUT && iptables-legacy -t nat -A OUTPUT -p tcp -d 203.0.113.7 --dport 80 -j REDIRECT --to-ports 15002 && (nc -l -p 15002 -e echo redirected > /dev/null 2>&1 &) && sleep 0.5 && wget -qO- -T 3 http://203.0.113.7/"
try sysctl_route_localnet "sysctl -w net.ipv4.conf.all.route_localnet=1"
'
docker run --rm --cap-add NET_ADMIN alpine:3.20 sh -c "$script" | while IFS='|' read -r step code out; do
  python3 -c 'import json,sys; print(json.dumps({"step": sys.argv[1], "exit": int(sys.argv[2]), "out": sys.argv[3]}))' "$step" "$code" "$out"
done
# The commands compiled into proxy-everything.
id=$(docker create cloudflare/proxy-everything:main)
mkdir -p /tmp/pe && docker export "$id" | tar -x -C /tmp/pe 2>/dev/null
docker rm "$id" > /dev/null
find /tmp/pe -type f -perm -u+x -size +1M | head -3 | while read -r bin; do
  strings "$bin" | grep -E '^(ip|iptables|ip6tables|nft|sysctl)( |$)|(-j (TPROXY|REDIRECT|DNAT))|route_localnet|fwmark' | sort -u | head -40 |
    python3 -c 'import json,sys; print(json.dumps({"binary": sys.argv[1], "commands": [l.strip() for l in sys.stdin]}))' "$bin"
done
