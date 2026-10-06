---
layout: default
title: ECS / Fargate
parent: Deployment
nav_order: 3
---

# ECS / Fargate Deployment

Run Rift beside your application in one ECS task, so the application's hard-coded HTTPS calls are
answered by the [intercept listener]({{ site.baseurl }}/features/intercept-proxy/).

> **A reference, not a tested path.** The task definition below is a starting point assembled from
> the container and intercept behaviour documented elsewhere. Nothing in this repository runs it on
> Fargate. The executable, CI-verified example of the same setup is the
> [HTTPS intercept demo](https://github.com/achird-labs/rift/tree/master/docs/demo#demo-6-https-intercept-proxy-standalone) (Docker
> Compose). Have the definition reviewed against the ECS schema before relying on it.

---

## Shape of the task

With `awsvpc` networking (mandatory on Fargate) every container in a task shares one network
namespace, so the app reaches Rift on `localhost` -- the same arrangement as the
[Kubernetes sidecar]({{ site.baseurl }}/deployment/kubernetes/#intercept-listener-as-a-sidecar).

- `rift` runs the intercept listener, with its rules declared in a config file.
- `app` sets `HTTPS_PROXY=http://127.0.0.1:8080` and trusts the intercept CA.
- The CA pair lives in Secrets Manager. ECS injects a secret only as an environment variable, and
  the config-file block reads it from there by name: `"caCertPemEnv": "INTERCEPT_CA_CERT"`,
  `"caKeyPemEnv": "INTERCEPT_CA_KEY"`. The variables are deliberately **not** named
  `RIFT_INTERCEPT_*`: those are the flags' own variables, and setting one alongside a config
  `intercept` block is a startup error.
- The app trusts the **same certificate**, so it must be baked into the app image (below) -- a JVM
  in particular reads its truststore once, at startup.

## Task definition

```json
{
  "family": "app-with-rift",
  "networkMode": "awsvpc",
  "requiresCompatibilities": ["FARGATE"],
  "cpu": "512",
  "memory": "1024",
  "executionRoleArn": "arn:aws:iam::123456789012:role/ecsTaskExecutionRole",
  "containerDefinitions": [
    {
      "name": "rift",
      "image": "123456789012.dkr.ecr.eu-west-1.amazonaws.com/app-rift-mocks:latest",
      "essential": true,
      "command": ["--configfile", "/config/mock.json"],
      "stopTimeout": 10,
      "secrets": [
        { "name": "INTERCEPT_CA_CERT",
          "valueFrom": "arn:aws:secretsmanager:eu-west-1:123456789012:secret:rift-intercept-ca-cert" },
        { "name": "INTERCEPT_CA_KEY",
          "valueFrom": "arn:aws:secretsmanager:eu-west-1:123456789012:secret:rift-intercept-ca-key" }
      ],
      "healthCheck": {
        "command": ["CMD", "rift", "healthcheck"],
        "interval": 5,
        "timeout": 3,
        "retries": 10,
        "startPeriod": 5
      }
    },
    {
      "name": "app",
      "image": "123456789012.dkr.ecr.eu-west-1.amazonaws.com/app:latest",
      "essential": true,
      "dependsOn": [{ "containerName": "rift", "condition": "HEALTHY" }],
      "environment": [
        { "name": "HTTPS_PROXY", "value": "http://127.0.0.1:8080" },
        { "name": "NO_PROXY", "value": "localhost,127.0.0.1" }
      ]
    }
  ]
}
```

`dependsOn: HEALTHY` on the built-in `rift healthcheck` means the app starts only once the listener
is up with its rules installed. The `intercept` block in `mock.json` is the one shown under
[Docker]({{ site.baseurl }}/deployment/docker/#intercepting-https-from-another-container); give it
`"host": "127.0.0.1"` here, since only the task's own app connects, `"port": 8080`,
`"caCertPemEnv": "INTERCEPT_CA_CERT"` and `"caKeyPemEnv": "INTERCEPT_CA_KEY"`. A multi-line PEM
survives intact: Secrets Manager stores it verbatim and Rift reads the variable as-is. No shell is
involved, so the `-static` image works too.

On a Rift release before `caCertPemEnv` existed, the block refuses the two keys. There, write the
secrets to files with a shell entrypoint (`"entryPoint": ["sh", "-c"]`, `"command": ["umask 077 &&
printf '%s' \"$INTERCEPT_CA_CERT\" > /tmp/ca-cert.pem && printf '%s' \"$INTERCEPT_CA_KEY\" >
/tmp/ca-key.pem && exec rift --configfile /config/mock.json"]`) and point `caCertPath`/`caKeyPath`
at them; that needs the default (Debian-based) image.

**Alternative: the flag path.** Start the listener with `--intercept-port 8080` and the secrets named
`RIFT_INTERCEPT_CA_CERT_PEM` / `RIFT_INTERCEPT_CA_KEY_PEM`, with **no** `intercept` block, and install
the rules with a one-shot `PUT /intercept/rules` against `localhost:2525`. That needs no shell, but
costs a bootstrap call and a window where the app's first calls can race the missing rules; with
`caCertPemEnv` the block path needs no shell either, so prefer it.

Remember that `execute-command` and the task role are separate from this: the execution role needs
`secretsmanager:GetSecretValue` on both secrets.

## Getting the config file in

`--configfile` takes a local file, so it has to be on the container's filesystem before Rift starts.
Two ways:

1. **Bake it into a custom image** (used above). Rules change by rebuilding and redeploying:
   ```dockerfile
   FROM zainalpour/rift-proxy:latest
   COPY mock.json /config/mock.json
   ```
2. **Fetch it from S3 with an init container.** Add a non-essential-after-exit container that copies
   the file to a shared volume, and make `rift` depend on it with `"condition": "SUCCESS"`:
   ```json
   { "name": "fetch-config", "image": "amazon/aws-cli", "essential": false,
     "command": ["s3", "cp", "s3://my-bucket/mock.json", "/config/mock.json"],
     "mountPoints": [{ "sourceVolume": "config", "containerPath": "/config" }] }
   ```
   with `volumes: [{ "name": "config" }]` on the task and the same `mountPoints` entry on `rift`,
   plus `"dependsOn": [{ "containerName": "fetch-config", "condition": "SUCCESS" }]`. The task role
   needs `s3:GetObject`. A running task can then pick up an edited file with `POST /admin/reload` on
   its admin port (`localhost:2525`), which re-applies the file's `intercept.rules`; the listener
   itself (host, port, CA) is fixed at boot.

## Putting the CA in the app image

Generate the CA once, outside the task, and store the two halves in Secrets Manager. In the CI job
that builds the **app** image, add the certificate to the image's truststore. Rift's offline
[`intercept-ca`]({{ site.baseurl }}/configuration/cli/#intercept-ca) does both steps without starting
a server:

```bash
rift intercept-ca generate --out-dir ./ca
aws secretsmanager create-secret --name rift-intercept-ca-cert --secret-string file://ca/ca-cert.pem
aws secretsmanager create-secret --name rift-intercept-ca-key  --secret-string file://ca/ca-key.pem

# JVM app: a truststore holding the CA plus the system roots
RIFT_TRUSTSTORE_PASSWORD=changeit \
  rift intercept-ca export --cert ./ca/ca-cert.pem --format jks --out ./truststore.jks \
    --merge-system-cas /etc/ssl/certs/ca-certificates.crt
```

Then `COPY ca/ca-cert.pem /certs/ca.pem` (or `truststore.jks`) into the app image and point the
runtime at it. Which variable adds to the default roots and which replaces them, and the JVM flags,
are under
[Trusting the CA from the SUT]({{ site.baseurl }}/features/intercept-proxy/#trusting-the-ca-from-the-sut).
Keep `ca-key.pem` out of the app image -- only Rift needs it.

## Stopping the task

ECS sends `SIGTERM` and, after `stopTimeout`, `SIGKILL`. Rift handles `SIGTERM` itself, as PID 1,
and exits `0` within about three seconds, so a short `stopTimeout` is enough and no init process is
needed -- see [Stopping the container]({{ site.baseurl }}/deployment/docker/#stopping-the-container).

## Notes

- **`forward` rules.** A `forward` action defaults to `127.0.0.1:{port}`, the Rift container's own
  loopback. An imposter in the same task is reachable there; one in another task or service needs
  `"host"` (and `"scheme"`): see
  [Forward to one of your imposters]({{ site.baseurl }}/features/intercept-proxy/#forward-to-one-of-your-imposters).
- **One listener per task.** Each task has its own listener and rules, as with Kubernetes replicas.
- **Keep the listener private.** Do not map port 8080 in a load balancer or open it in the security
  group; an unauthenticated intercept proxy is open to anything that can reach it. To expose it
  anyway, set the block's `auth` (`RIFT_INTERCEPT_AUTH` is a flag-path variable and conflicts with a block).
