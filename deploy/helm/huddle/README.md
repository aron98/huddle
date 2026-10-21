# huddle Helm chart

Deploys [huddle](https://github.com/aron98/huddle) — a single Rust binary
serving web, signaling, and SFU — as one pod per node.

The chart defaults to `hostNetwork: true` with hostPorts for web/signaling
(`service.port`, default 3000/tcp) and media (`server.mediaPort`, default
40000/udp): a Kubernetes Service cannot proxy SFU media for ICE, because the
SFU advertises its own address and browsers must send UDP SRTP directly to it.
Keep one replica per node — meeting state lives in the pod's memory, and each
replica is an independent island. See `docs/deploy/kubernetes.md` in the repo
for the full production guide (ingress, cert-manager, coturn companion).

## Configuration

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` / `image.tag` / `image.pullPolicy` | `ghcr.io/aron98/huddle` / `""` (= `appVersion`) / `IfNotPresent` | Container image. No CI workflow publishes the default repository — build and push it yourself. |
| `hostNetwork` | `true` | Pod shares the node network namespace (required for direct media). |
| `service.port` | `3000` | Web/signaling port (TCP). |
| `server.baseUrl` | `http://localhost:3000` | External URL used in join links, magic links, ICS invites. |
| `server.publicIp` | `""` | IP advertised in ICE candidates; empty = auto-detect the node IP. |
| `server.mediaPort` | `40000` | UDP media port. |
| `server.iceServers` | `""` | Extra ICE servers (your coturn) — lands in the ConfigMap; for credentials use `extraSecret`/`extraEnvFrom` instead (below). |
| `server.sessionSecret` / `existingSecret` | `""` | Session cookie HMAC key: value gate + name of the Secret holding `HUDDLE_SESSION_SECRET` (below). |
| `persistence.enabled` / `persistence.size` / `persistence.storageClass` | `true` / `1Gi` / `""` | PVC for SQLite + recordings. |
| `resources` | `{}` | Container resources. |
| `extraEnv` | `[]` | Extra env vars (below). |
| `extraEnvFrom` | `[]` | Extra envFrom sources (below). |
| `extraSecret` | `{}` | Opt-in chart-managed Secret (below). |

## Session secret

`HUDDLE_SESSION_SECRET` (the HMAC key for stateless session cookies) can be
delivered two ways:

- `server.sessionSecret: <value>` — the chart creates a Secret named
  `<fullname>-secret` with the value and points the pod at it. Convenient for
  `--set`, but the value then lives in chart values / CLI history.
- `existingSecret: <name>` — reference a Secret you created out-of-band (with
  a `HUDDLE_SESSION_SECRET` key); takes precedence when both are set, and the
  chart renders no Secret of its own.

With neither set, huddle falls back to a random key per boot (sessions do not
survive restarts).

> Migrating a release that set `server.sessionSecret` and hand-created
> `<fullname>-secret`? The chart now owns that name — delete your manual
> Secret before `helm upgrade`, or switch to `existingSecret` and keep it.

## Environment plumbing

The chart delivers its own settings (bind, `baseUrl`, media port, optional
`publicIp` and `iceServers`) through a ConfigMap consumed via `envFrom`. Three
additive knobs let you deliver anything else — including secrets — without
touching that path:

### `extraEnv` — literal env vars

A list of `{name, value}` or `{name, valueFrom}` entries appended to the
container's `env`. `env` entries take precedence over all `envFrom` sources, so
they can also override the chart's ConfigMap.

```yaml
extraEnv:
  - name: HUDDLE_MAIL_PROVIDER
    value: resend
  - name: HUDDLE_MAIL_RESEND_API_KEY
    valueFrom:
      secretKeyRef:
        name: huddle-mailer
        key: HUDDLE_MAIL_RESEND_API_KEY
```

### `extraEnvFrom` — Secret / ConfigMap sources

A list of `{type: Secret | ConfigMap, name, prefix (optional)}` entries
appended after the chart's ConfigMap in `envFrom`; on key collisions the last
source wins, so these entries override the ConfigMap.

```yaml
extraEnvFrom:
  - type: Secret
    name: huddle-ice
  - type: ConfigMap
    name: huddle-extra
    prefix: EXTRA_   # optional: prefixes every imported key
```

### `extraSecret` — opt-in chart-managed Secret

For small deployments that don't want a hand-rolled Secret manifest. When this
map is non-empty, the chart creates a Secret named
`<fullname>-extra-secret` (default fullname: `huddle`) with the keys as
`stringData`, and you load it with `extraEnvFrom`. When it is empty (default)
no Secret is rendered. Keep credentials here, not in `server.*` values — those
land in the plain ConfigMap.

```yaml
extraSecret:
  HUDDLE_ICE_SERVERS: '[{urls=["turn:turn.example.com:3478?transport=udp","turn:turn.example.com:3478?transport=tcp","turns:turn.example.com:5349?transport=tcp"],username="huddle",credential="REPLACE_WITH_TURN_PASSWORD"}]'
extraEnvFrom:
  - type: Secret
    name: huddle-extra-secret
```

> If you set `fullnameOverride`, the generated Secret is named
> `<fullnameOverride>-extra-secret` — reference that name in `extraEnvFrom`.
