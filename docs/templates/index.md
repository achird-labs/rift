---
layout: default
title: Templates
nav_order: 9.5
permalink: /templates/
---

# Vendor-Mock Templates

Ready-made Rift configurations that stand in for a third-party SaaS your system calls. The vendor's
own SDK keeps running unchanged — its decision logic, retries and event building are the real code —
and talks to Rift instead of the vendor's hosts:

- **Direct mode**: point the SDK at the mock's URLs (usually one or two settings).
- **Intercept mode**: change nothing in the code; set `HTTPS_PROXY` to Rift's
  [intercept proxy]({{ site.baseurl }}/features/intercept-proxy/) and trust its CA, the way
  `AWS_ENDPOINT_URL` points an AWS SDK at LocalStack.

Every imposter in a template records requests, so a test asserts on what the SDK sent through the
admin API (`savedRequests`, `verify`).

---

## Catalog

| Template | Stands in for | Ports | Modes | Verified against |
|:---------|:--------------|:------|:------|:-----------------|
| [Optimizely](https://github.com/achird-labs/rift/tree/master/templates/optimizely) | Optimizely Feature Experimentation: datafile CDN (`cdn.optimizely.com`), event ingest (`logx.optimizely.com`), ODP segments (`api.zaius.com`) | 4600–4602, intercept 4610 (block 4600–4619) | direct, intercept | `optimizely-sdk` 5.7.0 (Python) |

Each template's README covers what is mocked, per-language client wiring, the knobs (magic user ids,
API keys, validator stamps) and what is not covered.

---

## Getting a template

From a clone of the repository:

```bash
rift --configfile templates/optimizely/imposters.json
templates/optimizely/smoke.sh        # black-box checks; exits non-zero on the first failure
```

From a release: every release publishes the catalog as `rift-templates-<version>.tar.gz` with a
`.sha256` sidecar. Each template's manifest records that release as its minimum engine version.

```bash
tar -xzf rift-templates-<version>.tar.gz
rift --configfile rift-templates-<version>/optimizely/imposters.json
```

A template is a directory, not a single file: its entrypoint inlines fixtures with EJS
`<%- stringify('fixtures/…') %>`, which Rift refuses for `https:` sources. Extract it and load it
from disk; `--imposters https://…` cannot load it.

To validate one, lint the entrypoint **file** (`rift-lint templates/optimizely/imposters.json`), not
the directory — a directory lint reads the template's `template.json` manifest as an imposter.

---

## What every template guarantees

- **Declarative.** No `inject`, `decorate`, `shellTransform` or `_rift.script`; it loads on a plain
  `rift --configfile`, without `--allow-injection`.
- **Reserved ports.** Templates use 4600–4999, a 20-port block each, so templates can run side by
  side and next to the [examples]({{ site.baseurl }}/examples/) (4545–4550).
- **Gated in CI.** Each template is parsed and linted by the test suite, then booted on the built
  binary with its `smoke.sh` run against it, on every pull request.
- **Synthetic data.** Fixtures are written for the template; ids and keys are made up, and there are
  no real credentials.
- **One intercept block per rig.** Only one config source may declare `intercept`, so two templates
  that both carry one cannot be loaded as two sources; merge their `intercept.rules` into one file.

The full catalog rules, the directory layout and the `template.json` manifest format are in
[`templates/README.md`](https://github.com/achird-labs/rift/blob/master/templates/README.md).
