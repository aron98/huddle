# deploy/compose — production Docker Compose stack

Secure-by-default example for running huddle + coturn on a single Docker host.
**Full step-by-step guide: [`docs/deploy/docker-compose.md`](../../docs/deploy/docker-compose.md).**

```sh
cp .env.example .env    # fill in domains + secrets
docker compose build
docker compose up -d
```

The compose file at the **repo root** is the development stack (hot reload) —
do not use it in production.

Contents:

- `docker-compose.yml` — huddle (built from the root Dockerfile, UI embedded via
  rust-embed, built-in ACME TLS on 443) + coturn, both on host networking for
  WebRTC UDP reachability.
- `.env.example` — domains, public IP, `HUDDLE_SESSION_SECRET`, `HUDDLE_STATS_TOKEN`,
  TURN password.
- `turnserver.conf` — coturn template (lt-cred-mech static credentials to match
  huddle's `HUDDLE_ICE_SERVERS`).

`.env`, `.env.*` with real secrets, and `certs/` are git-ignored — never commit them.
