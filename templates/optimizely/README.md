# Optimizely Feature Experimentation — Rift mock template

A drop-in local replacement for the three Optimizely SaaS hosts an SDK (or Optimizely Agent) depends on. The SDK's own decision engine keeps running unchanged; the mock only serves what the SaaS would serve. That makes it high-fidelity for free: bucketing, audience evaluation, variables, forced decisions and event building are all the vendor's code, driven by a datafile you control.

| Port | Stands in for | Serves |
|---|---|---|
| 4600 | `https://cdn.optimizely.com` (and `config.optimizely.com/datafiles/auth/`) | `GET /datafiles/{sdkKey}.json` → 200 + `Last-Modified`; `If-Modified-Since` matching the current stamp → 304 |
| 4601 | `https://logx.optimizely.com` | `POST /v1/events` → 204 (400 if the batch has no `visitors[].visitor_id`) |
| 4602 | `https://api.zaius.com` (ODP) | `POST /v3/graphql` segment lookups, `POST /v3/events` identify events, 403 on a wrong `x-api-key` |
| 4610 | TLS intercept forward proxy | answers the hosts above over HTTPS from the three imposters (see [Intercept mode](#intercept-mode-zero-code-changes)) |

The template owns ports **4600–4619** of the catalog's reserved range (see [`../README.md`](../README.md)). Admin API on `http://localhost:2525` as usual. Every imposter records requests, so tests assert on what the SDK sent through the admin API.

## Files

```
imposters.json            Rift config file (the entrypoint): three imposters + an intercept block;
                          inlines the datafile with <%- stringify('fixtures/datafile.json') %>
fixtures/datafile.json    Synthetic datafile v4: 3 flags, 1 A/B test, 1 attribute audience,
                          1 ODP audience, ODP integration block
smoke.sh                  Black-box checks with curl + python3 (no SDK needed); CI runs it
sdk_check.py              Drives the real optimizely-sdk (Python) in direct mode
sdk_check_localstack.py   Drives the real optimizely-sdk (Python) in intercept mode, no code overrides
template.json             Manifest: ports, knobs, modes, what it was verified against
```

The datafile is synthetic: every id, key and the SDK key are made up. It carries the structure the SDKs need, nothing from a real Optimizely project.

## Run

```sh
rift --configfile imposters.json          # from this directory, or pass the path to it
./smoke.sh                                # exits non-zero on the first failed check
```

`stringify` reads the fixture relative to `imposters.json`, so the template works from any working directory as long as the directory is intact. To validate it, lint the entrypoint **file**: `rift-lint imposters.json`. Do not lint the directory: `rift-lint` would read `template.json` as an imposter.

Real-SDK checks (not run in CI: they need the network to install the SDK):

```sh
python3 -m venv .venv && .venv/bin/pip install optimizely-sdk requests
.venv/bin/python sdk_check.py
```

Edit `fixtures/datafile.json` (flip a rollout, change a variable, add a flag), then:

```sh
curl -X POST http://localhost:2525/admin/reload      # re-reads the file; only the CDN stub is patched
```

Pollers that cache `Last-Modified` keep receiving 304 until you also change the stamp in `imposters.json` (the `If-Modified-Since` predicate of the `datafile-not-modified` stub and the two `Last-Modified` headers). Bump `revision` in the datafile at the same time so `OptimizelyConfig.revision` moves. The two-stub 304 is a stand-in: once Rift ships a declarative conditional GET (`_rift.conditional`, achird-labs/rift#1280), the CDN imposter collapses to one stub whose validator follows the body.

## Intercept mode: zero code changes

`imposters.json` also declares an `intercept` block: a TLS forward proxy on **4610** that answers
`cdn.optimizely.com`, `config.optimizely.com`, `logx.optimizely.com`, `eu.logx.optimizely.com` and
`api.zaius.com` from the three imposters. The SDK keeps its production URLs; only the environment changes,
the same way `AWS_ENDPOINT_URL` points an AWS SDK at LocalStack.

```sh
OPTLY_ODP_HOST=https://api.zaius.com rift --configfile imposters.json &   # datafile now names the real ODP host
curl -s http://localhost:2525/intercept/ca.pem -o rift-ca.pem              # CA is regenerated per start; re-export
export HTTPS_PROXY=http://127.0.0.1:4610
export REQUESTS_CA_BUNDLE=$PWD/rift-ca.pem   # Python   | NODE_EXTRA_CA_CERTS (Node) | SSL_CERT_FILE (Go, OpenSSL)
.venv/bin/python sdk_check_localstack.py     # optimizely.Optimizely(sdk_key="MOCK_SDK_KEY") — nothing else
```

For a JVM: `curl "http://localhost:2525/intercept/truststore.jks?password=changeit" -o ts.jks` and
`-Djavax.net.ssl.trustStore=ts.jks -Dhttps.proxyHost=127.0.0.1 -Dhttps.proxyPort=4610`. For Optimizely Agent in
Docker, pass `HTTPS_PROXY` and mount the CA instead of the two `OPTIMIZELY_CLIENT_*` URL overrides.

- Each forwarded request reaches its imposter with the `Host` the SDK dialed (`cdn.optimizely.com`, …),
  so recordings say which vendor host was called.
- `POST /admin/reload` re-applies the block's `rules` along with the imposters.
- A CA that must be trusted *before* Rift starts (a container JVM that reads its truststore once):
  make a persistent one with `rift intercept-ca generate --out-dir ./ca` and set `caCertPath`/`caKeyPath`
  in the block. See the `intercept-ca` section of the CLI reference.
- Intercept mode needs a Rift release newer than 0.19.0 for Python 3.13+ clients (leaf certificates
  with an Authority Key Identifier) and for the dialed `Host` in recordings. Direct mode works on 0.19.0.

## Point a client at it

Two overrides per SDK: the datafile URL template and the event endpoint host. ODP needs **no** client override: the SDK reads the ODP host and key from the datafile's `integrations` block, and the fixture already says `http://localhost:4602` / `mock-odp-public-key`.

**Optimizely Agent** (Docker, nothing else changes; Agent's own REST API stays real):
```sh
docker run -p 8080:8080 \
  -e OPTIMIZELY_CLIENT_DATAFILEURLTEMPLATE='http://host.docker.internal:4600/datafiles/%s.json' \
  -e OPTIMIZELY_CLIENT_EVENTURL='http://host.docker.internal:4601/v1/events' \
  -e OPTIMIZELY_SDKKEYS=MOCK_SDK_KEY optimizely/agent
curl -H 'X-Optimizely-SDK-Key: MOCK_SDK_KEY' 'http://localhost:8080/v1/decide?keys=checkout_redesign' -d '{"userId":"user-7","userAttributes":{"plan":"premium"}}'
```

**Python**
```python
cm = PollingConfigManager(sdk_key="MOCK_SDK_KEY", url_template="http://localhost:4600/datafiles/{sdk_key}.json")
class MockDispatcher(EventDispatcher):
    @staticmethod
    def dispatch_event(event):
        requests.post(event.url.replace("https://logx.optimizely.com", "http://localhost:4601"),
                      data=json.dumps(event.params), headers=event.headers)
client = optimizely.Optimizely(config_manager=cm, event_dispatcher=MockDispatcher)
```

**JavaScript / Node (v5+)**
```js
const projectConfigManager = createPollingProjectConfigManager({ sdkKey: 'MOCK_SDK_KEY',
  urlTemplate: 'http://localhost:4600/datafiles/%s.json' });
const eventDispatcher = { dispatchEvent: e => fetch(e.url.replace('https://logx.optimizely.com', 'http://localhost:4601'),
  { method: 'POST', headers: {'content-type': 'application/json'}, body: JSON.stringify(e.params) }) };
```

**Java**
```java
ProjectConfigManager cm = HttpProjectConfigManager.builder().withSdkKey("MOCK_SDK_KEY")
    .withFormat("http://localhost:4600/datafiles/%s.json").build();
// events: the endpoint is fixed inside EventFactory; wrap the EventHandler and rewrite logEvent.getEndpointUrl()
```

**Go**
```go
cm := config.NewPollingProjectConfigManager("MOCK_SDK_KEY",
    config.WithDatafileURLTemplate("http://localhost:4600/datafiles/%s.json"))
// events: wrap event.Dispatcher and rewrite LogEvent.EndPoint; Go accepts only 204 as success (the mock returns 204)
```

A hosts-file / DNS override is **not** a shortcut: every SDK pins `https://` on 443. Use the intercept mode above to leave production code untouched.

## Scenarios built in

| Want to test | Do |
|---|---|
| Flag on for everyone, variable value | `decide("checkout_redesign")` for any user |
| A/B bucketing gated by an attribute audience | user with `plan=premium` → `checkout_redesign_ab`, control/treatment at 50/50 |
| Kill switch | `legacy_search` is off for everyone |
| ODP real-time segment targeting | `user-vip` qualifies for `high_value_customers` → `vip_support` on; anyone else off |
| ODP unknown identifier path | `user-unknown` → GraphQL `errors[]` with `InvalidIdentifierException` |
| ODP auth failure | send any `x-api-key` other than `mock-odp-public-key` → 403 |
| Conditional polling | send `If-Modified-Since: Sat, 03 Oct 2026 12:00:00 GMT` → 304 |
| Datafile change at runtime | edit fixture, `POST /admin/reload` |
| Event delivery assertion | `GET /imposters/4601/savedRequests?match=path=/v1/events`, or `POST /imposters/4601/verify` |

### Faults (paste into the relevant stub)

```jsonc
{ "is": { "statusCode": 200, "body": "..." }, "_rift": { "fault": { "error": { "probability": 0.3, "status": 503, "body": "cdn down" } } } }
{ "is": { "statusCode": 204 }, "_behaviors": { "wait": 15000 } }          // slow event ingest, exercises dispatcher timeouts/retries
{ "fault": "CONNECTION_RESET_BY_PEER" }                                    // CDN unreachable: SDK must fall back to cached datafile
```

## Not covered

- **Web Experimentation** (`cdn.optimizely.com/js/<projectId>.js` snippet) — a browser JS bundle, not a JSON API; out of scope.
- **CMAB** prediction endpoint (`prediction.cmab.optimizely.com/predict/{ruleId}`, SDKs ≥ 5.x) — add a 4th imposter if you use contextual bandits.
- **Datafile webhooks** to Agent — nothing stops you from `curl`-ing Agent's `/webhooks/optimizely` yourself after a reload.
- **Optimizely's REST/Admin APIs** (`api.optimizely.com/v2`, flag management) — a different product surface.
- The mock does not validate event batches beyond `visitors[].visitor_id`; the real ingest is stricter.
