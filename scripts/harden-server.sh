#!/usr/bin/env bash
#
# Harden a fresh Debian/Ubuntu VPS and prepare it as a Kamal deploy target.
# Runs as root ON the server. scripts/setup-deploy.sh runs it for you:
#
#   ssh root@HOST "bash -s -- '<deploy ssh public key>'" < scripts/harden-server.sh
#
# Idempotent: re-running converges to the same state. What it sets up and why:
# docs/deploy.md#server-baseline

set -euo pipefail

DEPLOY_USER=deploy
SSHD_DROPIN=/etc/ssh/sshd_config.d/00-tg-mcp.conf

log()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die()  { printf '\033[31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# write_file PATH MODE: replace PATH with stdin only if the content differs.
# Returns 0 when the file changed, 1 when it was already up to date.
write_file() {
  local path="$1" mode="$2" tmp
  tmp=$(mktemp)
  cat > "$tmp"
  if [[ -f "$path" ]] && cmp -s "$tmp" "$path"; then
    rm -f "$tmp"
    return 1
  fi
  install -D -m "$mode" "$tmp" "$path"
  rm -f "$tmp"
  echo "    wrote $path"
}

# ── Preflight ─────────────────────────────────────────────────────────────
DEPLOY_PUBKEY="${1:-}"
[[ -n "$DEPLOY_PUBKEY" ]] || die "usage: harden-server.sh '<deploy ssh public key>'"
[[ "$DEPLOY_PUBKEY" =~ ^(ssh-ed25519|ecdsa-sha2-nistp(256|384|521)|ssh-rsa)\ [A-Za-z0-9+/=]+( .*)?$ ]] \
  || die "the argument is not an SSH public key"
[[ $EUID -eq 0 ]] || die "run as root"

# shellcheck source=/dev/null
. /etc/os-release
case "${ID:-}" in
  debian|ubuntu) ;;
  *) die "unsupported OS '${ID:-unknown}': Debian or Ubuntu only" ;;
esac

# Turning password login off without a working key would lock you out.
grep -qE '^(ssh-|ecdsa-|sk-)' /root/.ssh/authorized_keys 2>/dev/null \
  || die "root has no key in /root/.ssh/authorized_keys. From your machine run: ssh-copy-id root@<this server>, then re-run."

export DEBIAN_FRONTEND=noninteractive
APT=(apt-get -q -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold)

# ── Packages ──────────────────────────────────────────────────────────────
log "Upgrading the system and installing base packages"
"${APT[@]}" update
"${APT[@]}" upgrade
"${APT[@]}" install ca-certificates curl gnupg openssh-server ufw fail2ban python3-systemd \
  unattended-upgrades apt-listchanges

# ── Docker CE from Docker's repo ──────────────────────────────────────────
log "Docker"
if dpkg -s docker.io >/dev/null 2>&1; then
  echo "    distro docker.io is installed; keeping it (remove it and re-run to switch to Docker CE)"
elif ! dpkg -s docker-ce >/dev/null 2>&1; then
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  write_file /etc/apt/sources.list.d/docker.list 0644 <<EOF || true
deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable
EOF
  "${APT[@]}" update
  "${APT[@]}" install docker-ce docker-ce-cli containerd.io docker-buildx-plugin
else
  echo "    docker-ce already installed"
fi

docker_changed=0
write_file /etc/docker/daemon.json 0644 <<'EOF' && docker_changed=1
{
  "live-restore": true,
  "no-new-privileges": true,
  "log-driver": "json-file",
  "log-opts": { "max-size": "20m", "max-file": "5" }
}
EOF
systemctl enable --now docker
if (( docker_changed )); then
  # live-restore keeps running containers up across this restart.
  systemctl restart docker
fi

# ── Deploy user ───────────────────────────────────────────────────────────
log "User '$DEPLOY_USER' (Kamal connects as this user)"
id "$DEPLOY_USER" >/dev/null 2>&1 || useradd --create-home --shell /bin/bash "$DEPLOY_USER"
usermod -aG docker "$DEPLOY_USER"
passwd -l "$DEPLOY_USER" >/dev/null   # key-only; no password to guess
DEPLOY_HOME=$(getent passwd "$DEPLOY_USER" | cut -d: -f6)
install -d -m 0700 -o "$DEPLOY_USER" -g "$DEPLOY_USER" "$DEPLOY_HOME/.ssh"
AUTH_KEYS="$DEPLOY_HOME/.ssh/authorized_keys"
touch "$AUTH_KEYS"
grep -qxF "$DEPLOY_PUBKEY" "$AUTH_KEYS" || { echo "$DEPLOY_PUBKEY" >> "$AUTH_KEYS"; echo "    added deploy key"; }
chown "$DEPLOY_USER:$DEPLOY_USER" "$AUTH_KEYS"
chmod 0600 "$AUTH_KEYS"

# ── SSH ───────────────────────────────────────────────────────────────────
log "SSH: keys only"
# sshd keeps the FIRST value it reads for each option, and drop-ins are read in
# name order, so 00- wins over e.g. cloud-init's 50-cloud-init.conf.
grep -qE '^\s*Include\s+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config \
  || sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
ssh_changed=0
write_file "$SSHD_DROPIN" 0644 <<EOF && ssh_changed=1
# Managed by tg-mcp scripts/harden-server.sh
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitEmptyPasswords no
PubkeyAuthentication yes
AuthenticationMethods publickey
PermitRootLogin prohibit-password
AllowUsers root $DEPLOY_USER
MaxAuthTries 3
LoginGraceTime 20
X11Forwarding no
ClientAliveInterval 300
ClientAliveCountMax 2
EOF
install -d -m 0755 /run/sshd
sshd -t || { rm -f "$SSHD_DROPIN"; die "sshd rejected the config; drop-in removed, nothing applied"; }
if (( ssh_changed )); then
  # reload keeps existing sessions (including this one) alive.
  systemctl reload ssh 2>/dev/null || systemctl restart ssh
fi

# ── Firewall ──────────────────────────────────────────────────────────────
log "Firewall (ufw): 22 rate-limited, 80, 443"
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
ufw limit 22/tcp comment 'ssh' >/dev/null
ufw allow 80/tcp comment 'http (kamal-proxy, ACME)' >/dev/null
ufw allow 443/tcp comment 'https (kamal-proxy)' >/dev/null
ufw --force enable >/dev/null
ufw status verbose | sed 's/^/    /'

# ── fail2ban ──────────────────────────────────────────────────────────────
log "fail2ban: sshd jail"
f2b_changed=0
write_file /etc/fail2ban/jail.d/sshd.local 0644 <<'EOF' && f2b_changed=1
[sshd]
enabled  = true
backend  = systemd
maxretry = 5
findtime = 10m
bantime  = 1h
EOF
systemctl enable --now fail2ban
(( f2b_changed )) && systemctl restart fail2ban

# ── Automatic security updates ────────────────────────────────────────────
log "Unattended upgrades: security + Docker, reboot at 04:30 UTC if needed"
write_file /etc/apt/apt.conf.d/20auto-upgrades 0644 <<'EOF' || true
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
APT::Periodic::AutocleanInterval "7";
EOF
write_file /etc/apt/apt.conf.d/52tg-mcp-unattended-upgrades 0644 <<'EOF' || true
// Managed by tg-mcp scripts/harden-server.sh
Unattended-Upgrade::Origins-Pattern { "origin=Docker,label=Docker CE"; };
Unattended-Upgrade::Remove-Unused-Dependencies "true";
Unattended-Upgrade::Automatic-Reboot "true";
Unattended-Upgrade::Automatic-Reboot-Time "04:30";
EOF
timedatectl set-timezone UTC 2>/dev/null || true

log "Done"
cat <<EOF
    Kamal target: $DEPLOY_USER@$(hostname -f 2>/dev/null || hostname)
    Before closing this session, check from ANOTHER terminal that you can still log in:
      ssh root@<server>                 # your personal key
      ssh $DEPLOY_USER@<server> docker ps   # the deploy key
EOF
