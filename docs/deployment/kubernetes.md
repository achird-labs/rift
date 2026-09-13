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
              path: /
              port: admin
            initialDelaySeconds: 5
            periodSeconds: 10
          readinessProbe:
            httpGet:
              path: /
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

---

## High Availability

### Multi-Replica Deployment

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
