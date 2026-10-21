# Deploying Huddle on an Ubuntu VPS

End-to-end, copy-pasteable guide for running a secure, production Huddle instance on
Ubuntu 22.04 or 24.04: the static release binary under systemd, TLS via the built-in
ACME client, coturn for TURN, a locked-down firewall, and a sandboxed service unit.

Every command below is meant to be run verbatim as root (or with `sudo`) on a fresh
Ubuntu 22.04/24.04 machine. Every port, path and environment variable has been
verified against the server code (`crates/server/src`) and the shipped configs
(`deploy/turn/turnserver.conf`, `Dockerfile`, `.github/workflows/release.yml`).

For hardening topics that apply to every platform (cookies, rate limits, backups,
upgrades, log hygiene), see [hardening-checklist.md](hardening-checklist.md) — this
guide focuses on the VPS-specific mechanics.

## Architecture overview

Huddle is deliberately a **single-binary deployment**:

- The release workflow builds a fully static musl binary with the web UI **baked in**
  (rust-embed, ADR 004). No Node, no asset directory to ship, no runtime deps beyond
  glibc-free musl itself.
- TLS is terminated **by the binary** (ADR 008): an in-process ACME client
  (rustls-acme) answers the Let's Encrypt `TLS-ALPN-01` challenge on the serving port
  and renews certificates automatically. Certificate and account state is cached under
  `<data_dir>/acme-cache` — you must not delete that directory between restarts.
- Data is a single SQLite database (`<data_dir>/huddle.sqlite3`). Schema migrations are
  embedded in the binary and run automatically at startup.
- Media flows over a **single UDP port** (`media_port`, default `40000`) via the SFU.
- coturn is a separate process (recommended for participants behind symmetric NATs and
  restrictive firewalls) and is the only piece with additional ports.

```
                       ┌───────────────────────────────┐
   Browser ── HTTPS ──▶│ huddle (443/tcp: web+WS+ACME) │
           ── SRTP ──▶ │        (40000/udp: SFU media)  │
                       └───────────────────────────────┘
                       ┌───────────────────────────────┐
   Browser ── TURN ──▶ │ coturn (3478/udp+tcp listening)│
                       │        (49160-49200/udp relay) │
                       │        (5349/tcp: TURN over   │
                       │         TLS, optional)         │
                       └───────────────────────────────┘
```

## Prerequisites

- Ubuntu 22.04 LTS or 24.04 LTS, root access.
- A domain, e.g. `meet.example.com` for Huddle and `turn.example.com` for coturn,
  each with an **A record** pointing at the VPS public IP.
  The ACME `TLS-ALPN-01` challenge requires `meet.example.com` to resolve to this
  machine before you start Huddle with TLS enabled.
- Outbound access to Let's Encrypt (`acme-v02.api.letsencrypt.org`).

## Port matrix

Open exactly these; nothing else needs to be reachable from the internet:

| Port | Protocol | Process | Purpose |
|------|----------|---------|---------|
| 443 | TCP | huddle | HTTPS web UI, WebSocket signaling, ACME TLS-ALPN-01 |
| 40000 | UDP | huddle | SFU media (ICE/DTLS/SRTP) |
| 3478 | UDP **and** TCP | coturn | TURN listening port |
| 5349 | TCP | coturn | TURN over TLS (`turns:`) — optional, TCP-only |
| 49160–49200 | UDP | coturn | TURN relay port range |

Notes, verified against `config.rs` and `deploy/turn/turnserver.conf`:

- `5349` is **TCP only**. A `turns:` URL implies `?transport=tcp`; coturn's DTLS
  listener shares the plain `3478/udp` listener and its certificate, it does not use
  `5349/udp`.
- Huddle's HTTP listener port is whatever you set `bind` to. With `[tls]` ACME mode
  it **must be `:443`** because the TLS-ALPN-01 challenge is answered on the serving
  port. The code default is `0.0.0.0:3000`, which is what you want only behind a
  reverse proxy (see [the proxy variant](#variant-tls-termination-at-a-reverse-proxy)).

## 1. Create a dedicated user and directories

```sh
adduser --system --group --home /var/lib/huddle --shell /usr/sbin/nologin huddle
install -d -o huddle -g huddle -m 0750 /var/lib/huddle /opt/huddle
```

- `/var/lib/huddle` — `data_dir`: SQLite database, ACME cache, recordings.
- `/opt/huddle` — the binary.

## 2. Install the release binary

The release workflow publishes a static musl tarball and a `.sha256` sidecar on every
`v*` tag. Pick the version from
<https://github.com/aron98/huddle/releases> and set it once:

```sh
VERSION=0.1.0   # adjust to the release you are installing
cd /tmp
curl -fLO "https://github.com/aron98/huddle/releases/download/v${VERSION}/huddle-${VERSION}-x86_64-unknown-linux-musl.tar.gz"
curl -fLO "https://github.com/aron98/huddle/releases/download/v${VERSION}/huddle-${VERSION}-x86_64-unknown-linux-musl.tar.gz.sha256"
sha256sum -c "huddle-${VERSION}-x86_64-unknown-linux-musl.tar.gz.sha256"
tar xzf "huddle-${VERSION}-x86_64-unknown-linux-musl.tar.gz"
install -m 0755 huddle /opt/huddle/huddle
```

The binary embeds the web client; there is no `web_dir` to configure and no other
files to install. (Building your own release binary is possible — the release
workflow is the reference — but the published artifact is recommended.)

## 3. Configure Huddle: `/opt/huddle/huddle.toml`

Huddle reads `huddle.toml` from its working directory (override with `HUDDLE_CONFIG`);
every key can also be set with a `HUDDLE_*` environment variable. The full key
reference is in the [root README](../../README.md#self-hosting); this is a complete
production example:

```sh
cat >/opt/huddle/huddle.toml <<'EOF'
# --- network ---------------------------------------------------------------
bind = "0.0.0.0:443"            # must be 443 for ACME (TLS-ALPN-01)
base_url = "https://meet.example.com"
public_ip = "203.0.113.10"       # your VPS public IP; omit to auto-detect
media_port = 40000              # SFU UDP media port

# --- storage ---------------------------------------------------------------
data_dir = "/var/lib/huddle"    # SQLite DB, ACME cache, recordings

# --- secrets ---------------------------------------------------------------
# Generate with: openssl rand -hex 32
# If unset, a random secret is generated per boot and all sessions are
# invalidated on every restart.
session_secret = "REPLACE_WITH_openssl_rand_hex_32"

# Bearer token protecting /metrics (see the hardening checklist).
stats_token = "REPLACE_WITH_openssl_rand_hex_16"

# --- TLS (built-in, ADR 008) ----------------------------------------------
[tls]
mode = "acme"

[tls.acme]
domains = ["meet.example.com"]
contact = "admin@example.com"    # optional; "mailto:" is prepended automatically

# Uncomment for a dry run against the Let's Encrypt staging CA first:
# staging = true

# --- rate limiting ---------------------------------------------------------
# Per-IP sliding window (60s) applied to magic-link requests AND new WebSocket
# connections. 30/min default; 0 disables.
rate_limit_per_min = 30

# --- TURN (coturn, installed below) ---------------------------------------
[[ice_servers]]
urls = [
  "turn:turn.example.com:3478?transport=udp",
  "turn:turn.example.com:3478?transport=tcp",
  "turns:turn.example.com:5349?transport=tcp",
]
username = "huddle"
credential = "REPLACE_WITH_turn_password"
EOF
chown root:huddle /opt/huddle/huddle.toml
chmod 0640 /opt/huddle/huddle.toml
```

Generate the two secrets and paste them in:

```sh
openssl rand -hex 32   # session_secret
openssl rand -hex 16   # stats_token
openssl rand -hex 16   # TURN password, used again in step 5
```

### Email delivery

Huddle sends single-use magic links; with the `log` provider they are only printed to
the journal. For real users configure one of:

```toml
# option A: plain SMTP
[smtp]
host = "mail.example.com"
port = 587
username = "huddle@example.com"
password = "REPLACE"
# starttls = true        # default true
# from = "huddle@example.com"

# option B: Resend HTTP API
[mail]
provider = "resend"
[mail.resend]
api_key = "REPLACE"
from = "Huddle <noreply@example.com>"
```

(If both `[smtp]` and `[mail.resend]` are present you must set `[mail] provider`
explicitly — startup fails otherwise, by design.)

**First boot tip:** uncomment `staging = true`, start once, confirm the log shows a
successfully issued (staging) certificate, then remove the line and restart. The
staging CA avoids Let's Encrypt production rate limits while you iterate on DNS and
firewall issues. The ACME state lives in `/var/lib/huddle/acme-cache` — keep it
across restarts and backups.

## 4. systemd unit with sandboxing: `/etc/systemd/system/huddle.service`

```ini
[Unit]
Description=Huddle video conferencing server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=huddle
Group=huddle
WorkingDirectory=/opt/huddle
ExecStart=/opt/huddle/huddle
Restart=on-failure
RestartSec=5

# --- capabilities -----------------------------------------------------------
# Binding :443 needs CAP_NET_BIND_SERVICE (media on :40000/udp and coturn's
# ports are all unprivileged). This is the ONLY capability the service needs.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

# --- file system ------------------------------------------------------------
# Read-only everywhere except the data dir; /tmp is private and cleaned.
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ReadWritePaths=/var/lib/huddle

# --- kernel hardening -------------------------------------------------------
NoNewPrivileges=yes
ProtectClock=yes
ProtectControlGroups=yes
ProtectHostname=yes
ProtectKernelLogs=yes
ProtectKernelModules=yes
ProtectKernelTunables=yes
ProtectProc=invisible
ProcSubset=pid
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
UMask=0027

[Install]
WantedBy=multi-user.target
```

```sh
systemctl daemon-reload
systemctl enable --now huddle
```

Check it came up and issued a certificate:

```sh
systemctl status huddle --no-pager
journalctl -u huddle -f          # until you see the ACME issuance log line
curl -fsS https://meet.example.com/api/health
# {"status":"ok"}
```

> **journald note:** Huddle logs to stdout/stderr and nothing else. Magic-link tokens
> are never logged in full by the server (they are stored SHA-256-hashed, and the
> `log` mail provider prints the full link — use it only in development). journald
> applies rate-limiting and rotation by default; add e.g.
> `SystemMaxUse=500M` under `[Journal]` in `/etc/systemd/journald.conf` if you want
> a hard cap.

## 5. Firewall

`ufw` (verify it is active afterwards; the `before` matters — deny-by-default is the
point):

```sh
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp                        # adjust to your SSH policy FIRST
ufw allow 443/tcp comment 'huddle HTTPS + WS + ACME'
ufw allow 40000/udp comment 'huddle SFU media'
ufw allow 3478/udp comment 'coturn TURN'
ufw allow 3478/tcp comment 'coturn TURN (tcp)'
ufw allow 5349/tcp comment 'coturn TURN over TLS'
ufw allow 49160:49200/udp comment 'coturn relay range'
ufw enable
ufw status verbose
```

The equivalent nftables ingress policy, if you prefer it over ufw:

```sh
nft add table inet filter
nft add chain inet filter input '{ type filter hook input priority 0 ; policy drop ; }'
nft add rule inet filter input ct state established,related accept
nft add rule inet filter input iif lo accept
nft add rule inet filter input tcp dport 22 accept
nft add rule inet filter input tcp dport { 443, 3478, 5349 } accept
nft add rule inet filter input udp dport { 40000, 3478 } accept
nft add rule inet filter input udp dport 49160-49200 accept
```

> If you use both ufw and nftables, pick one — conflicting baseline policies are a
> classic source of "works locally, blackholes remotely" bugs.

## 6. coturn

### 6.1 Install and enable

```sh
apt update && apt install -y coturn
sed -i 's/#TURNSERVER_ENABLED=1/TURNSERVER_ENABLED=1/' /etc/default/coturn
```

### 6.2 Configure `/etc/turnserver.conf`

Huddle advertises TURN servers with **static username/credential pairs**
(`[[ice_servers]]` in `huddle.toml`), so coturn must validate them with
`lt-cred-mech` + a matching `user=` line. (`use-auth-secret` — the shared-secret
REST-credential scheme — is what many guides recommend, but Huddle has no
REST-auth credential helper, so with `use-auth-secret` participants could never
authenticate. The shipped template `deploy/turn/turnserver.conf` notes the same.)

```sh
cat >/etc/turnserver.conf <<'EOF'
# Huddle production TURN template — see deploy/turn/turnserver.conf in the repo.
listening-port=3478
tls-listening-port=5349

# Relay port range — must match the firewall rules (49160-49200/udp).
min-port=49160
max-port=49200

# Static credentials, mirrored exactly in huddle.toml [[ice_servers]].
lt-cred-mech
realm=turn.example.com
user=huddle:REPLACE_WITH_turn_password

# Certificate for TURN over TLS (5349/tcp) and DTLS on the UDP listener.
# Provisioned in 6.3; comment these two lines out until the files exist.
cert=/etc/turnserver/certs/turn.example.com/fullchain.pem
pkey=/etc/turnserver/certs/turn.example.com/privkey.pem

# Hardening.
fingerprint
no-cli
no-multicast-peers
# Never enable allow-loopback-peers. It is a relay-to-localhost hole.
EOF
chmod 0640 /etc/turnserver.conf
```

### 6.3 TLS certificate for coturn (optional but recommended)

coturn's certificate is **separate** from Huddle's: the built-in ACME client
provisions certificates for `meet.example.com` into its own cache
(`/var/lib/huddle/acme-cache`) and does not write PEM files for coturn. TURN over
TLS needs a certificate for `turn.example.com` in plain PEM form. The simplest route
that does not fight Huddle for port 443 is the DNS-01 challenge:

```sh
apt install -y certbot
# with your DNS provider's plugin, e.g.:
# apt install -y python3-certbot-dns-cloudflare
certbot certonly --standalone -d turn.example.com --pre-hook 'systemctl stop coturn' --post-hook 'systemctl start coturn'
```

> The `--standalone` challenge binds **tcp/80**, which nothing else in this stack
> uses, so it can run while Huddle is live. Alternatively use a DNS-01 plugin and
> drop the hooks entirely.

Then let the turnserver user read the certs (certbot writes them root-only by
default; the clean approach is a deploy hook that copies instead of symlink-chasing
into `/etc/letsencrypt/live`):

```sh
install -d -o turnserver -g turnserver -m 0750 /etc/turnserver/certs/turn.example.com
cat >/etc/letsencrypt/renewal-hooks/deploy/turn.sh <<'EOF'
#!/bin/sh
install -o turnserver -g turnserver -m 0640 \
  /etc/letsencrypt/live/turn.example.com/fullchain.pem /etc/turnserver/certs/turn.example.com/fullchain.pem
install -o turnserver -g turnserver -m 0640 \
  /etc/letsencrypt/live/turn.example.com/privkey.pem /etc/turnserver/certs/turn.example.com/privkey.pem
systemctl reload coturn
EOF
chmod +x /etc/letsencrypt/renewal-hooks/deploy/turn.sh
/etc/letsencrypt/renewal-hooks/deploy/turn.sh   # initial copy
systemctl restart coturn
systemctl enable coturn
```

coturn re-reads the PEM files on `systemctl reload coturn`, so the deploy hook
above is all that is needed for renewals.

If you skip this section, comment out `cert=`/`pkey=` and `tls-listening-port` in
`/etc/turnserver.conf` and remove the `turns:` URL from `huddle.toml` — plain
`turn:turn.example.com:3478` over UDP and TCP still works for participants.

### 6.4 Verify TURN

```sh
turnutils_uclient -u huddle -w REPLACE_WITH_turn_password -p 3478 turn.example.com
# or from a browser: https://ice-test.appspot.com/ (tracer) / webrtc internals
```

In a Huddle meeting, open `chrome://webrtc-internals` on the remote participant and
confirm the selected candidate pair is not `relay` for healthy networks (TURN relay
should only engage behind symmetric NATs), but that a relay path exists when you
force it — the e2e suite's `?turn=relay` mode is the reference behavior.

## 7. Verify the full stack

```sh
# 1. health
curl -fsS https://meet.example.com/api/health
# 2. metrics are now token-gated (stats_token above)
curl -fsS -H "Authorization: Bearer REPLACE_WITH_openssl_rand_hex_16" https://meet.example.com/metrics
# 3. browser: open https://meet.example.com, request a magic link, join a call
```

Run a two-party call between two networks (e.g. laptop on LAN + phone on mobile
data). If the remote side connects but shows no video, re-check the UDP rules —
missing `40000/udp` or the `49160-49200/udp` relay range are the usual culprits.

## Variant: TLS termination at a reverse proxy

Huddle's built-in TLS (above) is the recommended shape on a VPS: fewer moving parts,
automatic renewal, and the server then enforces HSTS and `Secure` cookies itself
(see [hardening-checklist.md](hardening-checklist.md)). A reverse proxy in front is
legitimate when you want WAF-style rules, multiple vhosts on one IP, or organizational
standardization — ADR 008 calls this "demoted, not banned". Huddle then serves plain
HTTP on loopback and the proxy owns 443. **Documented trade-offs:**

1. **HSTS moves to the proxy.** Huddle only emits
   `Strict-Transport-Security: max-age=31536000; includeSubDomains` when it terminates
   TLS itself; behind a proxy it never sends the header. Add it in the proxy.
2. **`Secure` cookies stay on.** Cookie security is keyed off `base_url` starting with
   `https://`, so keep `base_url = "https://meet.example.com"` and cookies are still
   marked `Secure; HttpOnly; SameSite=Lax`.
3. **The per-IP rate limiter sees only the proxy.** Rate limiting uses the socket peer
   address; with no `X-Forwarded-For` handling, every request appears to come from
   `127.0.0.1` and shares one 30/min budget. Either rate-limit at the proxy, or
   disable the server-side limiter (`rate_limit_per_min = 0`) so it can't be
   exhausted-pointlessly. Do not leave both unconfigured.
4. **ACME state no longer lives in Huddle** — the proxy (certbot/caddy) owns
   certificates; set `[tls]` off entirely (just omit the section).

### nginx example

```sh
apt install -y nginx
```

`/etc/nginx/sites-available/huddle`:

```nginx
server {
    listen 443 ssl;
    server_name meet.example.com;

    ssl_certificate     /etc/letsencrypt/live/meet.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/meet.example.com/privkey.pem;
    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        # WebSocket upgrade for /ws
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;   # long-lived signaling
    }
}
server {
    listen 80;
    server_name meet.example.com;
    return 301 https://$host$request_uri;
}
```

Then in `huddle.toml`: `bind = "127.0.0.1:3000"`, keep
`base_url = "https://meet.example.com"`, delete the `[tls]` section, and do **not**
open 3000 in the firewall (loopback only):

```sh
ln -sf /etc/nginx/sites-available/huddle /etc/nginx/sites-enabled/
rm -f /etc/nginx/sites-enabled/default
certbot --nginx -d meet.example.com
nginx -t && systemctl reload nginx
systemctl restart huddle
```

### caddy example

Caddy gives you ACME + HSTS + proxying in one file (`/etc/caddy/Caddyfile`):

```caddy
meet.example.com {
    header Strict-Transport-Security "max-age=31536000; includeSubDomains"
    reverse_proxy 127.0.0.1:3000
}
```

`bind = "127.0.0.1:3000"` in `huddle.toml` as with nginx; CARRY OUT the same
rate-limiting decision (point 3 above) — Caddy's `rate_limit` module or an
`execute`-based gate belongs in front.

## Upgrades

1. Back up `/var/lib/huddle` (see the [hardening checklist](hardening-checklist.md)).
2. Download and verify the new tarball exactly as in step 2.
3. `systemctl stop huddle && install -m 0755 huddle /opt/huddle/huddle && systemctl start huddle`
4. Watch the journal: SQL migrations run automatically at startup
   (`sqlx::migrate!`, embedded in the binary). There are no migration files to run
   by hand, and no downgrade path — hence the backup.

Meetings are in-memory; the restart drops all ongoing calls. The default systemd
`Restart=` policy does not apply to planned restarts — do them in a maintenance
window.

## Troubleshooting quick reference

| Symptom | Check |
|---------|-------|
| Startup fails, `both [tls.acme] and [tls.certs] present` | Remove the unused section or set `tls.mode` explicitly |
| Startup fails, cert/key file not found (`static` mode) | `[tls.certs]` paths must exist at boot — fail-fast by design |
| ACME challenge never arrives | `bind` must be `0.0.0.0:443`; DNS A record must point here; 443/tcp open |
| Local video works, remote black | UDP blocked: check 40000/udp and 49160-49200/udp in the firewall |
| Everyone logged out after a restart | `session_secret` not set — sessions are HMAC'd stateless cookies and restarts re-key them |
| 429 "too many connections" for all users behind a proxy | Per-IP limiter sees only the proxy address — rate-limit at the proxy or disable in-server limiting (see trade-off 3) |
| /metrics 401 | Set `stats_token` / send the bearer token; or you disabled token-gating — re-read the [checklist](hardening-checklist.md) |
