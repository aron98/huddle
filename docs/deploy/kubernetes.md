# Deploying Huddle on Kubernetes

Production guide for the Helm chart in [`deploy/helm/huddle`](../../deploy/helm/huddle/):
web/signaling through cert-manager + ingress, SFU media staying on hostNetwork
(non-negotiable), coturn as a companion DaemonSet, TURN credentials from a Secret.

All YAML below assumes: Kubernetes 1.27+, `helm` 3.x, `kubectl`, and ingress-nginx +
cert-manager for the HTTPS plane. Cross-platform topics live in
[hardening-checklist.md](hardening-checklist.md).

> The chart is linted in CI (`helm lint`) and is the deployment source of truth;
> where this guide and chart values disagree, the chart wins.

## The one hard constraint: SFU media cannot go through a Service

The SFU advertises an ICE candidate consisting of its own address. A browser
must send UDP SRTP **directly** to that advertised address:port. A Kubernetes
Service (ClusterIP/NodePort/LoadBalancer) proxies traffic through kube-proxy
conntrack and rewrites addresses — the browser would send to the Service, while the
SFU advertised the node IP, and ICE connectivity checks land nowhere. UDP
"session" semantics also have no reconnect story under conntrack churn.

The chart therefore defaults to:

```yaml
hostNetwork: true   # pod shares the node network namespace
```

with `hostPort` 3000/tcp for web/signaling and `mediaPort` 40000/udp for media, and
`dnsPolicy: ClusterFirstWithHostNet`. The Service the chart creates is a DNS
convenience for the ingress backend **only** — it never carries media.

Practical consequences:

- **One replica per node.** SFU meeting state is in memory and not sharded; two
  replicas on one node would fight over the same hostPorts. Scaling out means more
  pods on more nodes, each an independent "island" with its own join URL — see
  [Scaling](#scaling).
- **Node firewall / security groups must open** `40000/udp` (media) to the internet.
- The pod's PVC is per-island: each replica has its own SQLite + recordings.

## Prerequisites: the container image

The Helm chart references `ghcr.io/aron98/huddle`, which is published
automatically: pushing a `v*` tag builds the image from `Dockerfile.release`
(multi-stage: web client built with trunk, dist copied into
`crates/server/assets`, static musl server binary on alpine with
ca-certificates only) and pushes it to GHCR — see
`.github/workflows/release-image.yml`. Image tags mirror release tags without
the leading `v` (release v1.2.3 → image tag `1.2.3`); `latest` follows the
newest release. Images exist from the first release tag published after the
image workflow landed.

```yaml
image:
  repository: ghcr.io/aron98/huddle
  tag: "1.2.3"        # defaults to the chart's appVersion — pin explicitly
```

> The chart's `appVersion` follows the newest release automatically: pushing
> a `v*` tag triggers `.github/workflows/chart-sync.yml`, which sets
> `appVersion` to the tag (minus `v`), minor-bumps the chart `version`, and
> pushes the commit to `main` right after the release. The default-branch
> chart therefore always references the newest published image. Pin
> `image.tag` anyway when you want to control upgrade timing.

Self-hosting the image in your own registry (or building with local patches)
works exactly as before — build and push the repo root `Dockerfile`
(glibc/debian variant) or `Dockerfile.release` (musl/alpine variant, what CI
publishes), then point `image.repository`/`image.tag` at it:

```sh
docker build -f Dockerfile.release -t <registry>/huddle:1.2.3 .
docker push <registry>/huddle:1.2.3
```

## 1. Install cert-manager and ingress-nginx

Huddle's web plane (login, magic links, WebSocket signaling) is plain HTTP(S) and
works fine behind a normal L7 ingress — ingress-nginx upgrades WebSockets out of
the box. **Media never traverses the ingress.**

```sh
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx
helm install ingress-nginx ingress-nginx/ingress-nginx \
  --namespace ingress-nginx --create-namespace

helm repo add jetstack https://charts.jetstack.io
helm install cert-manager jetstack/cert-manager \
  --namespace cert-manager --create-namespace --set crds.enabled=true
```

> TLS termination at the ingress is the chart-compatible path. Huddle's built-in
> ACME (ADR 008) binds the *serving* port for TLS-ALPN-01, i.e. the pod would need
> `HUDDLE_BIND=0.0.0.0:443` + hostPort 443 — the chart's port plumbing
> (hostPort, readiness probe, Service) assumes `service.port`, so pod-terminated
> TLS still needs a small chart extension (see
> [Pod-terminated TLS variant](#pod-terminated-tls-variant-chart-extension-required)).
> The `HUDDLE_TLS_*` env vars themselves can already be passed via the chart's
> `extraEnv` knob.

## 2. Secrets

The session secret (HMAC key for stateless session cookies) belongs in a Secret.
The production path is `existingSecret` — a Secret created out-of-band:

```sh
kubectl create secret generic huddle-session \
  --from-literal=HUDDLE_SESSION_SECRET="$(openssl rand -hex 32)"
```

For quick setups, setting `server.sessionSecret` makes the chart create the
`<fullname>-secret` Secret for you — but the value then sits in your values
file or `--set` history, so prefer `existingSecret` in production (see the
[chart README](../../deploy/helm/huddle/README.md)).

TURN credentials also belong in a Secret — mounted by coturn in step 5, and fed
to huddle via the chart's `extraEnvFrom` knob so `HUDDLE_TURN_SHARED_SECRET`
never lands in the chart's ConfigMap (see steps 3 and 5).

The same plumbing carries mailer credentials. For example, a Resend API key:

```sh
kubectl create secret generic huddle-mailer -n huddle \
  --from-literal=HUDDLE_MAIL_RESEND_API_KEY="re_xxxxxxxxxxxx"
```

```yaml
# my-values.yaml
extraEnv:
  - name: HUDDLE_MAIL_PROVIDER
    value: resend
extraEnvFrom:
  - type: Secret
    name: huddle-mailer
```

(The chart can also create the Secret for you — `extraSecret` — see the
[chart README](../../deploy/helm/huddle/README.md).)

## 3. Install the Huddle chart

`my-values.yaml`:

```yaml
# image: repository/tag default to ghcr.io/aron98/huddle at the chart's
# appVersion — pin an explicit release tag here unless you ride the default:
# image:
#   tag: "1.2.3"

hostNetwork: true          # default — do not change, see the UDP constraint

server:
  # External URL used in join links, magic links, ICS invites.
  baseUrl: https://meet.example.com
  # IP advertised in ICE candidates. Empty = auto-detect the node IP.
  publicIp: ""
  mediaPort: 40000
  # TURN servers advertised to clients — URLs only: with the REST-auth
  # shared secret (section 5) huddle mints time-limited credentials at
  # every join, so no credentials are stored here. Static pairs work
  # too — then they must match coturn's user= byte-for-byte.
  # Figment array syntax, TOML-ish:
  iceServers: '[{urls=["turn:turn.example.com:3478?transport=udp","turn:turn.example.com:3478?transport=tcp","turns:turn.example.com:5349?transport=tcp"]}]'

# Pod env vars injected from Secrets (chart 0.2.0 knob): the TURN
# REST-auth shared secret (huddle-turn, step 5) and the mailer API
# key (huddle-mailer, step 2) — keep secret material out of the
# plain ConfigMap.
extraEnvFrom:
  - type: Secret
    name: huddle-turn
  - type: Secret
    name: huddle-mailer

# Secret with key HUDDLE_SESSION_SECRET (created above):
existingSecret: huddle-session

persistence:
  enabled: true
  size: 10Gi          # SQLite + recordings + (unused in this variant) acme-cache
  storageClass: ""    # pick one that plays well with one-pod-per-node (below)

resources:
  requests:
    cpu: 500m
    memory: 512Mi
```

> **Storage caveat:** the deployment uses `strategy: Recreate` (a rolling restart
> would kill all in-flight meetings) and one replica per node. Use a
> storageClass/zone topology that reattaches the same volume to the same node
> (e.g. a storageclass with `volumeBindingMode: WaitForFirstConsumer` and node-local
> or zonal provisioners). A cluster-wide RWX claim defeats the isolation anyway.

```sh
helm install huddle deploy/helm/huddle -f my-values.yaml -n huddle --create-namespace
kubectl -n huddle rollout status deploy/huddle
```

The chart's `NOTES.txt` reminds you of the same three gotchas: set `baseUrl`, open
the media UDP port on the nodes, and set `publicIp` if auto-detect picks the wrong
interface.

## 4. Ingress + certificate

Issuer (Let's Encrypt, HTTP-01 through ingress-nginx):

```sh
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@example.com
    privateKeySecretRef:
      name: letsencrypt-account-key
    solvers:
      - http01:
          ingress:
            class: nginx
EOF
```

Ingress — note the two proxy-related headers/trade-offs from the
[hardening checklist](hardening-checklist.md#tls-hsts-and-cookies), which apply
verbatim here:

```sh
kubectl apply -f - <<'EOF'
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: huddle
  namespace: huddle
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt
    # HSTS belongs to the TLS terminator — huddle only sends the header when
    # it terminates TLS itself, which is not this variant.
    nginx.ingress.kubernetes.io/force-ssl-redirect: "true"
spec:
  ingressClassName: nginx
  tls:
    - hosts: [meet.example.com]
      secretName: huddle-tls
  rules:
    - host: meet.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: huddle
                port:
                  number: 3000
EOF
```

Then point DNS `meet.example.com` at the ingress LB IP and verify:

```sh
kubectl -n huddle get ingress huddle
curl -fsS https://meet.example.com/api/health     # {"status":"ok"}
```

**Rate-limiting trade-off (important):** huddle's per-IP limiter keys on the socket
peer and does not read `X-Forwarded-For`. Behind the ingress, *all clients share the
ingress pod's IP* for the magic-link + WebSocket budget (default 30/min). Either
rate-limit at the ingress (annotations like
`nginx.ingress.kubernetes.io/limit-rps` on login routes) or raise/disable the
server-side limit. Do not leave both unconfigured.

## 5. coturn companion DaemonSet

TURN relay ports (49160–49200/udp) cannot be proxied by a Service any more than
SFU media can, so coturn also runs hostNetwork — as a DaemonSet on the same nodes
(or a dedicated set of nodes via nodeSelector/labels).

**Credential model:** huddle supports coturn's `use-auth-secret` (TURN REST
API, issue #59): huddle holds the same shared secret and mints time-limited
credentials at every join — `username` is the expiry timestamp and
`credential` is `base64(HMAC-SHA1(secret, username))` — so no static TURN
password is stored anywhere. The chart's `extraEnvFrom` knob (added alongside
this guide) delivers `HUDDLE_TURN_SHARED_SECRET` to the pod from the
`huddle-turn` Secret created below — through chart values, no post-install
patching.

Create the TURN shared secret and a coturn config referencing it (the config is
mounted as a Secret so the credential never appears in a ConfigMap):

```sh
TURN_SHARED_SECRET="$(openssl rand -hex 32)"
kubectl create secret generic huddle-turn -n huddle \
  --from-literal=HUDDLE_TURN_SHARED_SECRET="${TURN_SHARED_SECRET}"
kubectl create secret generic coturn-config -n huddle \
  --from-literal=turnserver.conf="listening-port=3478
tls-listening-port=5349
min-port=49160
max-port=49200
use-auth-secret
static-auth-secret=${TURN_SHARED_SECRET}
realm=turn.example.com
fingerprint
no-cli
no-multicast-peers
"
```

The chart's `extraEnvFrom` (step 3) injects `HUDDLE_TURN_SHARED_SECRET` from
the `huddle-turn` Secret into the pod, so the shared secret never appears in
plain chart values or the ConfigMap.

> With REST auth, `server.iceServers` carries only URL lists — safe to keep in
> the chart's ConfigMap (it holds no credentials). The shared secret lives in a
> Secret, which is not readable cluster-wide by default — strictly better than
> the old static-password-in-ConfigMap shape. Prefer static `lt-cred-mech`
> pairs instead? Then the same static password must appear in huddle's
> `HUDDLE_ICE_SERVERS`; keep it out of the ConfigMap by putting the whole value
> under the chart's `extraSecret` map and pointing `extraEnvFrom` at
> `huddle-extra-secret` (the chart then manages the Secret for you; see the
> [chart README](../../deploy/helm/huddle/README.md)).

DaemonSet manifest:

```sh
kubectl apply -f - <<'EOF'
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: coturn
  namespace: huddle
spec:
  selector:
    matchLabels: {app: coturn}
  template:
    metadata:
      labels: {app: coturn}
    spec:
      hostNetwork: true
      dnsPolicy: ClusterFirstWithHostNet
      # run only on nodes meant to relay TURN:
      nodeSelector:
        huddle.example.com/turn: "true"
      securityContext:
        runAsNonRoot: false    # coturn binds 3478/5349 (unprivileged); tighten per your image
      containers:
        - name: coturn
          image: coturn/coturn:latest
          args: ["-c", "/etc/coturn/turnserver.conf"]
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
          volumeMounts:
            - name: config
              mountPath: /etc/coturn
              readOnly: true
      volumes:
        - name: config
          secret:
            secretName: coturn-config
EOF
```

TURN over TLS (the `turns:…:5349` URL) additionally needs a PEM cert for the TURN
domain mounted read-only into the container — provision it outside huddle (certbot
DNS-01 → `kubectl create secret generic coturn-certs`), uncomment `cert=`/`pkey=`
in the config above, and drop the turns URL until it exists. See the
[VPS guide's coturn TLS section](ubuntu-vps.md#63-tls-certificate-for-coturn-optional-but-recommended)
for the certificate mechanics — they are identical.

**Advertising the TURN address:** with a DaemonSet, every node runs coturn, but
huddle advertises a *static* URL list. Either give the TURN nodes stable DNS names
and list them all in `server.iceServers` (step 3 — with the REST-auth shared
secret huddle mints the credentials, and the URL list holds no secrets, so the
chart ConfigMap is fine; browsers try every candidate), or run a single coturn
pinned to one node with a stable public IP and advertise just that one. Both
work; the single-coturn shape is easier to reason about at small scale.

## 6. Node firewall / security groups

Per node running huddle (+ coturn):

| Port | Protocol | Direction | Purpose |
|------|----------|-----------|---------|
| 443 | TCP | via ingress LB | HTTPS/WSS (terminated at ingress) |
| 3000 | TCP | from ingress controller pods only | plain HTTP backend |
| 40000 | UDP | 0.0.0.0/0 | SFU media (hostNetwork pod) |
| 3478 | UDP + TCP | 0.0.0.0/0 | TURN listening |
| 5349 | TCP | 0.0.0.0/0 | TURN over TLS (optional) |
| 49160–49200 | UDP | 0.0.0.0/0 | TURN relay |

## Scaling

`replicas > 1` (across distinct nodes) gives you **islands**, not a cluster: each
pod is an independent SFU with its own SQLite volume and its own meetings. There is
no cross-pod meeting handover — joining routes to one island, and that pod's
restart ends its meetings. To spread load, publish per-island hostnames
(`meet-1.example.com`, `meet-2.example.com` — one Helm release + ingress each) and
distribute join URLs yourself. The chart's `strategy: Recreate` exists for the same
reason: a rolling update is indistinguishable from a crash for every in-flight
meeting.

## Upgrades

```sh
helm upgrade huddle deploy/helm/huddle -f my-values.yaml -n huddle
kubectl -n huddle rollout status deploy/huddle
```

- The deployment strategy is `Recreate`: the old pod dies, then the new one starts.
  All meetings on that island drop. Treat upgrades as a maintenance window.
- Schema migrations run automatically at startup (`sqlx::migrate!` embedded in the
  binary). No downgrade path — back up the PVC first
  ([hardening checklist](hardening-checklist.md#backups)).
- Image tags: the chart defaults `image.tag` to `appVersion`; pin an explicit tag
  and update it deliberately.

## Pod-terminated TLS variant (chart extension required)

If you need huddle itself to own TLS on the cluster — e.g. no ingress, direct node
exposure — you can use the built-in ACME (ADR 008). The env side is already
covered: the chart's `extraEnv` knob passes `HUDDLE_TLS_MODE=acme`,
`HUDDLE_TLS_DOMAINS`, `HUDDLE_TLS_CONTACT`, and friends. What is still missing
from the chart is port plumbing (a small extension if you take it on):

1. `HUDDLE_BIND=0.0.0.0:443` via `extraEnv` (it overrides the ConfigMap's
   bind), plus the matching `containerPort`/`hostPort` and Service port —
   the TLS-ALPN-01 challenge is answered on the serving port.
2. Readiness still probes `/api/health` over plain HTTP — the probe needs a
   port/mode that works when the serving port speaks TLS.

Until then, the ingress path above is the supported shape on Kubernetes; the
[VPS guide](ubuntu-vps.md) covers the binary-owns-443 topology natively.

## Known chart limitations (as of this guide)

1. ~~`image.repository` defaults to `ghcr.io/aron98/huddle`, which no CI workflow
   publishes~~ — resolved: `release-image.yml` publishes it on every `v*` tag
   (tag minus `v`, plus `latest`).
2. ~~The chart's `appVersion` only advanced when the chart itself was released,
   so it lagged the newest image tag (issue #71)~~ — resolved: `chart-sync.yml`
   updates `Chart.yaml` on every `v*` tag and pushes the bump to `main`.

The earlier gaps — no `extraEnv`/`extraEnvFrom` knob and TURN credentials
landing in the plain ConfigMap — were resolved by the chart 0.2.0 plumbing
this guide now uses. If you spot another gap, file an issue so it lands
upstream rather than as a local fork.

## Troubleshooting quick reference

| Symptom | Check |
|---------|-------|
| Pod `Pending`: nodePort collision | Another pod holds 3000/40000 on the node — one replica per node |
| Join works, remote media black | Node security group: 40000/udp open? `publicIp` detected the right interface? |
| ICE relay never engages | coturn DaemonSet scheduled (nodeSelector)? TURN URL list matches coturn's realm, and the `HUDDLE_TURN_SHARED_SECRET` patch matches `static-auth-secret`? |
| Ingress 502 on `/ws` | Ingress class nginx upgrades WS automatically; check the backend Service port is 3000 and the pod is Ready |
| 429 "too many connections" for everyone | All traffic shares the ingress pod IP — see the rate-limit trade-off in step 4 |
| Certificates stuck `False` | cert-manager solver can't reach the challenge — check ingress LB IP and DNS A record |
