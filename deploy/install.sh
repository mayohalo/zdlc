#!/usr/bin/env bash
# Installs or updates the escrow room on an Ubuntu server, as root:
#   deploy/install.sh DOMAIN
# expects the binaries in /opt/zdlc/bin (scripts/deploy.sh copies them). Sets up a
# zdlc user, swap, a firewall that allows only SSH and HTTPS, Caddy for TLS, and the
# desk, two oracles and the room under systemd. Every service listens on localhost.
set -euo pipefail
DOMAIN="${1:?usage: install.sh DOMAIN}"
HERE="$(cd "$(dirname "$0")" && pwd)"

id zdlc >/dev/null 2>&1 || useradd --system --home /var/lib/zdlc --shell /usr/sbin/nologin zdlc
install -d -o zdlc -g zdlc -m 700 /var/lib/zdlc /var/lib/zdlc/room
install -d -m 755 /etc/zdlc
[ -f /var/lib/zdlc/admin.token ] || { head -c 16 /dev/urandom | xxd -p >/var/lib/zdlc/admin.token; chown zdlc:zdlc /var/lib/zdlc/admin.token; chmod 600 /var/lib/zdlc/admin.token; }
[ -f /etc/zdlc/zdlc.env ] || install -m 640 -g zdlc "$HERE/zdlc.env.example" /etc/zdlc/zdlc.env

# 1 GB of memory: a swap file keeps a proof from running out.
if ! swapon --show | grep -q /swapfile; then
  fallocate -l 1G /swapfile && chmod 600 /swapfile && mkswap /swapfile >/dev/null && swapon /swapfile
  grep -q /swapfile /etc/fstab || echo '/swapfile none swap sw 0 0' >>/etc/fstab
fi

ufw allow OpenSSH >/dev/null && ufw allow 80/tcp >/dev/null && ufw allow 443/tcp >/dev/null && ufw --force enable >/dev/null

if ! command -v caddy >/dev/null; then
  apt-get update -qq && apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl gnupg >/dev/null
  curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt >/etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq && apt-get install -y -qq caddy >/dev/null
fi
sed "s/^DOMAIN /$DOMAIN /" "$HERE/Caddyfile" >/etc/caddy/Caddyfile

install -m 644 "$HERE"/systemd/*.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now zdlc-desk zdlc-oracle@7801 zdlc-oracle@7802 >/dev/null
systemctl enable zdlc-room >/dev/null
systemctl reload caddy || systemctl restart caddy
if grep -q PRIVATE-NODE-ADDRESS /etc/zdlc/zdlc.env; then
  echo "set ZDLC_RPC in /etc/zdlc/zdlc.env, then: systemctl restart zdlc-desk zdlc-room"
else
  # Every service runs the binaries just installed (the oracles' state is sealed on disk).
  systemctl restart zdlc-desk zdlc-oracle@7801 zdlc-oracle@7802 zdlc-room
fi
echo "installed: https://$DOMAIN/"
