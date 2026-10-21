# Deploying Huddle with Docker Compose

Production huddle + coturn stack on a single Docker host: the release image built
from the repo `Dockerfile` (web client baked into the binary via rust-embed), TLS via
huddle's built-in ACME client, coturn alongside, host networking for WebRTC media.

All example files live in [`deploy/compose/`](../../deploy/compose/) in the repo:

| File | Purpose |
|------|---------|
| `deploy/compose/docker-compose.yml` | Production stack (huddle + coturn) |
| `deploy/compose/.env.example` | Template for secrets/domains |
| `deploy/compose/turnserver.conf` | coturn config template |

> The compose file at the **repo root** is the *development* stack — trunk watch,
> hot reload, dev TURN credentials. It is intentionally not hardened and should not
> face the internet. Keeping the production stack in `deploy/compose/` leaves the
> root file free to optimize for the inner dev loop.

Commands below are verified for Docker Engine with the compose plugin (v2) on
Ubuntu 22.04/24.04. Cross-platform hardening (cookies, rate limits, backups,
upgrades) is covered in [hardening-checklist.md](hardening-checklist.md).

## Why host networking

The SFU must advertise an ICE candidate that is *directly* reachable on UDP —
a NAT'd bridge candidate is a classic cause of "connects, but no video". This stack
uses `network_mode: host` for both services, same rationale as the dev compose
file: docker's bridge NAT makes WebRTC UDP flaky, and built-in ACME needs the real
`443/tcp` for the TLS-ALPN-01 challenge. Host networking also means **ufw rules
apply to container traffic normally** — with published (`ports:`) containers
docker programs the `DOCKER` iptables chain *ahead* of ufw and silently bypasses it.

## Port matrix

Open exactly these on the host firewall (same as the
[VPS guide](ubuntu-vps.md#port-matrix)):

| Port | Protocol | Service | Purpose |
|------|----------|---------|---------|
| 443 | TCP | huddle | HTTPS + WebSocket + ACME |
| 40000 | UDP | huddle | SFU media |
| 3478 | UDP + TCP | coturn | TURN listening |
| 5349 | TCP | coturn | TURN over TLS (optional) |
| 49160–49200 | UDP | coturn | TURN relay range |

## 1. Configure

```sh
cd deploy/compose
cp .env.example .env
# generate secrets:
openssl rand -hex 32   # -> HUDDLE_SESSION_SECRET
openssl rand -hex 16   # -> HUDDLE_STATS_TOKEN
openssl rand -hex 16   # -> TURN_PASSWORD
${EDITOR:-vi} .env
```

Write the TURN password into the coturn template (its config file cannot read
environment variables):

```sh
source .env
sed -i "s/REPLACE_WITH_TURN_PASSWORD/${TURN_PASSWORD}/" turnserver.conf
```

Point `HUDDLE_DOMAIN` (and `TURN_DOMAIN`) DNS A records at the host first — the
ACME TLS-ALPN-01 challenge requires the domain to resolve here before huddle starts.

## 2. Build and start

```sh
docker compose build          # builds the root Dockerfile: trunk web build + release server
docker compose up -d
docker compose logs -f huddle # wait for the ACME issuance log line
curl -fsS https://meet.example.com/api/health
# {"status":"ok"}
```

The `huddle-data` volume holds `/data` inside the container: SQLite database
(`<data_dir>/huddle.sqlite3`), the ACME certificate/account cache
(`<data_dir>/acme-cache`) and recordings. It must survive restarts and be part of
backups — see the [hardening checklist](hardening-checklist.md#backups).

> **No healthcheck, on purpose.** The runtime image is debian:bookworm-slim with
> ca-certificates only (no curl/wget), so an in-container HTTP probe would need a
> bigger, less minimal image. Monitor `/api/health` and `/metrics` (bearer
> `HUDDLE_STATS_TOKEN`) from outside, e.g. uptime-kuma or a k8s-style probe later.

## 3. coturn TLS (optional)

TURN over TLS (`turns:…:5349`) needs a PEM certificate for the TURN domain —
huddle's ACME cache only serves huddle's own domain and is not readable by coturn.
Provision it on the host exactly as in the [VPS guide](ubuntu-vps.md#63-tls-certificate-for-coturn-optional-but-recommended)
(certbot `--standalone` on tcp/80 or DNS-01), copy the PEM files into
`deploy/compose/certs/turn.example.com/`, then:

1. Uncomment the `certs` volume bind and the `cert=`/`pkey=` lines in
   `deploy/compose/turnserver.conf`.
2. Uncomment the `./certs:/etc/coturn/certs:ro` mount in `docker-compose.yml`.
3. `docker compose up -d coturn` (recreate).

## Variant: bridge network + reverse proxy

If host networking is not allowed on your platform (some managed docker hosts,
shared environments), run huddle on a bridge with published ports and put a
TLS-terminating proxy in front. Know the trade-offs, all of which also apply to the
[VPS proxy variant](ubuntu-vps.md#variant-tls-termination-at-a-reverse-proxy):

- **HSTS must be set at the proxy** — huddle only emits the header when it
  terminates TLS itself.
- **The per-IP rate limiter only sees the proxy's container IP.** Every client
  shares one 30/min budget for magic links + WebSocket connections, because the
  limiter keys on the socket peer and huddle does not read `X-Forwarded-For`.
  Rate-limit at the proxy, or set `HUDDLE_RATE_LIMIT_PER_MIN=0`.
- **Keep `HUDDLE_BASE_URL=https://…`** regardless, so cookies are still marked
  `Secure`.
- The proxy terminates TLS (e.g. certbot/caddy), so huddle runs `[tls]`-less
  (`bind` = `127.0.0.1:3000` in the VPS layout; on a bridge,
  `HUDDLE_BIND=0.0.0.0:3000` and publish `3000/tcp` to loopback only:
  `ports: ["127.0.0.1:3000:3000"]`).
- Media still needs `ports: ["40000:40000/udp"]` and a correctly set
  `HUDDLE_PUBLIC_IP` — this is the flaky double-NAT path; test it with a remote
  participant, not just localhost.

## Upgrades

```sh
git fetch --tags && git checkout v0.1.0   # or the version you deploy
docker compose build
docker compose up -d                       # recreates huddle; drops in-flight meetings
docker compose logs huddle | grep -i migrate
```

Schema migrations run automatically at startup (`sqlx::migrate!` is embedded in the
binary — no migration files to copy). There is no downgrade path: back up the
`huddle-data` volume first.

## Troubleshooting quick reference

| Symptom | Check |
|---------|-------|
| `HUDDLE_DOMAIN: required variable not set` | You skipped `cp .env.example .env` / didn't fill a value |
| ACME challenge fails | DNS A record; `HUDDLE_BIND` must be `0.0.0.0:443`; 443/tcp reachable |
| Local video, remote black | `40000/udp` + `49160-49200/udp` firewall rules; coturn relay range matches `min/max-port` |
| TURN auth fails (401 in coturn log) | `user=` in turnserver.conf must match `username`/`credential` in `HUDDLE_ICE_SERVERS` byte-for-byte |
| Cert errors after restart | `huddle-data` volume was recreated — the acme-cache must persist |
| Everyone logged out after restart | `HUDDLE_SESSION_SECRET` unset or changed |
