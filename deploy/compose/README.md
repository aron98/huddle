# deploy/compose — production Docker Compose stack

Secure-by-default example for running huddle + coturn on a single Docker host.
**Full step-by-step guide: [`docs/deploy/docker-compose.md`](../../docs/deploy/docker-compose.md).**

```sh
cp .env.example .env    # fill in domains + secrets + HUDDLE_IMAGE_TAG
docker compose pull huddle
docker compose up -d
```

The compose file at the **repo root** is the development stack (hot reload) —
do not use it in production.

Contents:

- `docker-compose.yml` — huddle (published `ghcr.io/aron98/huddle` image, UI
  embedded via rust-embed, built-in ACME TLS on 443; self-build via the root
  Dockerfile documented in the guide) + coturn, both on host networking for
  WebRTC UDP reachability.
- `.env.example` — domains, public IP, `HUDDLE_IMAGE_TAG`,
  `HUDDLE_SESSION_SECRET`, `HUDDLE_STATS_TOKEN`, TURN shared secret.
- `turnserver.conf` — coturn template (`use-auth-secret` REST credentials matching
  huddle's `HUDDLE_TURN_SHARED_SECRET`).

`.env`, `.env.*` with real secrets, and `certs/` are git-ignored — never commit them.
