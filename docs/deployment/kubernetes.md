---
layout: default
title: Kubernetes
parent: Deployment
nav_order: 2
---

# Kubernetes Deployment

Deploy Rift in Kubernetes for production mock services and chaos engineering.

---

## Quick Start

### Basic Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rift
  labels:
    app: rift
spec:
  replicas: 1
  selector:
    matchLabels:
      app: rift
  template:
    metadata:
      labels:
        app: rift
    spec:
      containers:
        - name: rift
          image: zainalpour/rift-proxy:latest
          ports:
            - name: admin
              containerPort: 2525
            - name: metrics
              containerPort: 9090
          env:
            - name: MB_PORT
              value: "2525"
            - name: MB_ALLOW_INJECTION
              value: "true"
          resources:
            requests:
              memory: "128Mi"
              cpu: "250m"
            limits:
              memory: "512Mi"
              cpu: "1000m"
          livenessProbe:
            httpGet:
              path: /health
              port: admin
            initialDelaySeconds: 5
            periodSeconds: 10
          readinessProbe:
            httpGet:
              path: /health
              port: admin
            initialDelaySeconds: 5
            periodSeconds: 5
---
apiVersion: v1
kind: Service
metadata:
  name: rift
spec:
  selector:
    app: rift
  ports:
    - name: admin
      port: 2525
      targetPort: admin
    - name: metrics
      port: 9090
      targetPort: metrics
```

### Probes and `--api-key`

With `MB_APIKEY` / `--api-key` set, every admin API path — `/health` included — answers `401`
without the key, so a plain `httpGet` probe on the admin port fails. Use `rift healthcheck` as an
`exec` probe instead: it presents the key the container already holds from `MB_APIKEY` (issue
#1154), so the secret stays in the pod's environment rather than being copied into the manifest the
way an `httpGet` `httpHeaders` entry would put it:

```yaml
          livenessProbe:
            exec:
              command: ["rift", "healthcheck"]
            timeoutSeconds: 3
          readinessProbe:
            exec:
              command: ["rift", "healthcheck"]
            timeoutSeconds: 3
```

Set `timeoutSeconds` explicitly: an `exec` probe defaults to 1s, while `rift healthcheck` waits up
to 2s by default, so without it a slow `/health` is killed by the kubelet instead of being reported
by the probe. The key must come from the pod's environment (`MB_APIKEY`, e.g. from a Secret) — the
probe is a separate process and does not see the container's `args`.

The image's own Docker `HEALTHCHECK` is ignored by Kubernetes, so declare the probe explicitly as
above. Probing the metrics port with `httpGet` still works too, but it only proves the metrics
listener is up, not that the admin API is healthy.

### Shutdown

A pod stops promptly: `rift` handles the `SIGTERM` the kubelet sends, including as the container's
PID 1 (issue #1155), so the default `terminationGracePeriodSeconds` is ample — the server's own
shutdown is bounded at about three seconds. It exits `0` and leaves any `--datadir` state on its
volume. No init process and no shortened grace period are needed.

---

## Configuration with ConfigMap

### ConfigMap for Imposters

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: rift-imposters
data:
  imposters.json: |
    {
      "imposters": [
        {
          "port": 4545,
          "protocol": "http",
          "name": "User Service Mock",
          "stubs": [
            {
              "predicates": [{ "equals": { "path": "/health" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "OK" } }]
            },
            {
              "predicates": [{ "equals": { "path": "/users" } }],
              "responses": [{
                "is": {
                  "statusCode": 200,
                  "headers": { "Content-Type": "application/json" },
                  "body": [{ "id": 1, "name": "Test User" }]
                }
              }]
            }
          ]
        }
      ]
    }
```

### Deployment with ConfigMap

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rift
spec:
  template:
    spec:
      containers:
        - name: rift
          image: zainalpour/rift-proxy:latest
          args: ["--configfile", "/config/imposters.json"]
          ports:
            - name: admin
              containerPort: 2525
            - name: imposter
              containerPort: 4545
          volumeMounts:
            - name: config
              mountPath: /config
              readOnly: true
      volumes:
        - name: config
          configMap:
            name: rift-imposters
```

---

## TLS Configuration

### TLS Secret

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: rift-tls
type: kubernetes.io/tls
data:
  tls.crt: <base64-encoded-cert>
  tls.key: <base64-encoded-key>
```

### HTTPS Imposter

`<%- stringify('…') %>` inlines a file's contents into the JSON string when the config is loaded;
an absolute path is used as-is, a relative one is resolved against the config file's directory.

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: rift-https-config
data:
  imposters.json: |
    {
      "imposters": [{
        "port": 4545,
        "protocol": "https",
        "key": "<%- stringify('/tls/tls.key') %>",
        "cert": "<%- stringify('/tls/tls.crt') %>",
        "stubs": [...]
      }]
    }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rift
spec:
  template:
    spec:
      containers:
        - name: rift
          image: zainalpour/rift-proxy:latest
          args: ["--configfile", "/config/imposters.json"]
          volumeMounts:
            - name: config
              mountPath: /config
            - name: tls
              mountPath: /tls
              readOnly: true
      volumes:
        - name: config
          configMap:
            name: rift-https-config
        - name: tls
          secret:
            secretName: rift-tls
```

---

## Sidecar Pattern

### Application with Rift Sidecar

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app
spec:
  template:
    spec:
      containers:
        # Main application
        - name: app
          image: my-app:latest
          env:
            - name: EXTERNAL_API_URL
              value: "http://localhost:4545"

        # Rift sidecar
        - name: rift
          image: zainalpour/rift-proxy:latest
          args: ["--configfile", "/config/imposters.json"]
          ports:
            - containerPort: 4545
          volumeMounts:
            - name: mock-config
              mountPath: /config

      volumes:
        - name: mock-config
          configMap:
            name: my-app-mocks
```

### Intercept listener as a sidecar

To mock an external HTTPS host the app hard-codes, run the
[intercept listener]({{ site.baseurl }}/features/intercept-proxy/) in the sidecar. Containers in a pod
share a network namespace, so the app reaches it on `127.0.0.1`. Keep the CA pair in a `Secret`
(made with `rift intercept-ca generate --out-dir ca`, see
[`intercept-ca`]({{ site.baseurl }}/configuration/cli/#intercept-ca)). Rift reads it from a mounted
volume through the config block's `caCertPath` / `caKeyPath`, and the app trusts the certificate from
its own mount. Do not also set `RIFT_INTERCEPT_*` env vars on the sidecar: with an `intercept` block
that is a startup error.

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: rift-intercept-ca
stringData:
  ca-cert.pem: |
    -----BEGIN CERTIFICATE-----
    ...
  ca-key.pem: |
    -----BEGIN PRIVATE KEY-----
    ...
---
# in the Deployment's pod spec
securityContext:
  fsGroup: 1000                  # rift runs as uid 1000; lets it read the 0440 key
containers:
  - name: app
    image: my-app:latest
    env:
      - name: HTTPS_PROXY
        value: http://127.0.0.1:8080
      - name: NO_PROXY
        value: localhost,127.0.0.1,.svc,.cluster.local   # in-cluster hosts bypass the proxy
      - name: SSL_CERT_FILE
        value: /certs/ca-cert.pem
    volumeMounts:
      - name: ca
        mountPath: /certs
        readOnly: true
  - name: rift
    image: zainalpour/rift-proxy:latest
    args: ["--configfile", "/config/mock.json"]   # holds the `intercept` block and its rules
    volumeMounts:
      - name: mock-config
        mountPath: /config
      - name: ca-pair
        mountPath: /ca
        readOnly: true
volumes:
  - name: ca
    secret:
      secretName: rift-intercept-ca
      items: [{ key: ca-cert.pem, path: ca-cert.pem }]   # the app never gets the key
  - name: ca-pair
    secret:
      secretName: rift-intercept-ca
      defaultMode: 0440
      items:
        - { key: ca-cert.pem, path: ca-cert.pem }
        - { key: ca-key.pem, path: ca-key.pem }
  - name: mock-config
    configMap:
      name: my-app-mocks
```

The `intercept` block in `mock.json` is the one shown under
[Docker]({{ site.baseurl }}/deployment/docker/#intercepting-https-from-another-container), with
`"caCertPath": "/ca/ca-cert.pem"` and `"caKeyPath": "/ca/ca-key.pem"`; the listener can bind
`127.0.0.1` here since only the pod's own app connects. Other trust variables
(`NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, a JVM truststore) and which of them replace the default
roots: [Trusting the CA from the SUT]({{ site.baseurl }}/features/intercept-proxy/#trusting-the-ca-from-the-sut).
A `forward` rule reaches the sidecar's own `127.0.0.1` unless it names a `host`
([details]({{ site.baseurl }}/features/intercept-proxy/#forward-to-one-of-your-imposters)).

Each replica has its own listener and its own rules, like everything else in a replicated Rift (see
[High Availability](#high-availability)). Editing the ConfigMap and calling `POST /admin/reload`
re-applies the rules; the listener itself is boot-only.

---

## High Availability

### Multi-Replica Deployment

Each replica is an independent Rift: an imposter created through the admin API exists only on the
replica that received the request, and so do recorded requests. Behind a `Service`, admin calls and
verification land on arbitrary pods. Replicate only when every pod loads the same imposters at
startup (`--configfile` from a ConfigMap) and nothing mutates them at runtime; a Redis
`flowState` backend shares flow state between pods, not imposters.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rift
spec:
  replicas: 3
  template:
    spec:
      affinity:
        podAntiAffinity:
          preferredDuringSchedulingIgnoredDuringExecution:
            - weight: 100
              podAffinityTerm:
                labelSelector:
                  matchLabels:
                    app: rift
                topologyKey: kubernetes.io/hostname
      containers:
        - name: rift
          image: zainalpour/rift-proxy:latest
```

### Horizontal Pod Autoscaler

The same caveat applies: a pod the autoscaler adds starts with only the startup config.

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: rift
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: rift
  minReplicas: 2
  maxReplicas: 10
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 70
```

---

## Monitoring

### ServiceMonitor for Prometheus

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: rift
spec:
  selector:
    matchLabels:
      app: rift
  endpoints:
    - port: metrics
      interval: 15s
      path: /metrics
```

### PodMonitor

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PodMonitor
metadata:
  name: rift
spec:
  selector:
    matchLabels:
      app: rift
  podMetricsEndpoints:
    - port: metrics
      interval: 15s
```

---

## Namespace Isolation

### Dedicated Namespace

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: test-mocks
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rift
  namespace: test-mocks
spec:
  # ... deployment spec
```

### Network Policy

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: rift-policy
  namespace: test-mocks
spec:
  podSelector:
    matchLabels:
      app: rift
  policyTypes:
    - Ingress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              name: test-runners
      ports:
        - port: 2525
        - port: 4545
```

---

## Helm Chart (Example)

Rift does not publish a Helm chart. The values below are a starting point for a chart of your own;
map `config.allowInjection` to `MB_ALLOW_INJECTION`, `config.logLevel` to `MB_LOGLEVEL`, and
`imposters` to a ConfigMap passed with `--configfile`.

### values.yaml

```yaml
replicaCount: 1

image:
  repository: zainalpour/rift-proxy
  tag: latest
  pullPolicy: IfNotPresent

service:
  type: ClusterIP
  adminPort: 2525
  metricsPort: 9090

resources:
  limits:
    cpu: 1000m
    memory: 512Mi
  requests:
    cpu: 250m
    memory: 128Mi

config:
  allowInjection: true
  logLevel: info

imposters: |
  {
    "imposters": []
  }
```

---

## Troubleshooting

### Check Pod Status

```bash
kubectl get pods -l app=rift
kubectl describe pod -l app=rift
kubectl logs -l app=rift
```

### Port Forward for Testing

```bash
kubectl port-forward svc/rift 2525:2525
curl http://localhost:2525/imposters
```

### Debug Container

```bash
kubectl exec -it deployment/rift -- /bin/sh
```

The default image has a shell but no `curl`; the `-static` image has neither. For the static image,
use an ephemeral debug container instead:

```bash
kubectl debug -it <rift-pod-name> --image=busybox --target=rift
```
