# Vendor-mock templates

Ready-made Rift configurations that stand in for a third-party SaaS your system calls: the vendor's
SDK keeps running unchanged and talks to Rift instead of the real hosts, either because you point it
at the mock's URLs (direct mode) or because it reaches them through Rift's TLS intercept proxy
(intercept mode, no code change).

Each template is a directory, not a single file: an entrypoint config, the fixtures it inlines, a
smoke test and a manifest. Load it with `--configfile`:

```sh
rift --configfile templates/optimizely/imposters.json
templates/optimizely/smoke.sh
```

Every release also publishes the catalog as `rift-templates-<version>.tar.gz` (with a `.sha256`).
Extract it and load a template from the extracted directory:

```sh
tar -xzf rift-templates-<version>.tar.gz
rift --configfile rift-templates-<version>/optimizely/imposters.json
```

A template is a local-directory artifact: its entrypoint pulls fixtures in with EJS `stringify`,
which Rift refuses for `https:` sources, so it cannot be loaded straight from a URL.

## Catalog

| Template | Stands in for | Ports | Modes |
|---|---|---|---|
| [`optimizely/`](optimizely/) | Optimizely Feature Experimentation: datafile CDN, event ingest (logx), ODP segments | 4600–4619 (intercept 4610) | direct, intercept |

The same table, with what each template was verified against, is on the documentation site's
Templates page.

## Layout

```
templates/<name>/
  imposters.json      entrypoint: the imposters (+ an optional intercept block)
  fixtures/           data the entrypoint inlines with <%- stringify('fixtures/…') %>
  smoke.sh            black-box checks against a running rift; required
  sdk_check*.py       checks that drive the vendor's real SDK; recommended, not gated
  README.md           what is mocked, ports, per-language wiring, knobs, limits
  template.json       manifest (below); never read by the engine
```

## Rules

A template is accepted into the catalog when it follows these rules. The ones marked *(gated)* are
enforced in CI by `crates/rift-mock-core/tests/shipped_templates_load.rs`,
`crates/rift-lint/tests/cli_output.rs` (`the_shipped_templates_lint_clean`) and
`scripts/verify-templates.sh`.

- **Declarative only** *(gated)*. No `inject`, `decorate`, `shellTransform` or `_rift.script`, and
  `requires.flags` is `[]`. A template must load on a plain `rift --configfile`, without
  `--allow-injection`.
- **Loads and lints clean** *(gated)*. The entrypoint renders, parses and passes
  `rift-lint <name>/imposters.json`. Lint the entrypoint **file**, never the directory: a directory
  lint reads `template.json` as an imposter and fails.
- **Ports from the reserved range** *(gated)*. Templates use 4600–4999, 20 ports per template,
  recorded in the manifest's `ports` (`first`/`last`). Every imposter port and the intercept port sit
  inside that block, and no port is used by another template, by `examples/` (4545–4550) or by
  `docs/demo/`, so templates can be loaded side by side and next to the examples.
- **Recordable** *(gated)*. `recordRequests: true` on every imposter: asserting on what the SUT sent,
  through the admin API, is the point of a vendor mock. Name each imposter after the vendor host it
  stands in for (`cdn.optimizely.com`).
- **Smoke-tested** *(gated)*. `smoke.sh` uses only `curl` and `python3`, reads the base URLs from env
  vars (`ADMIN` for the admin API, defaulting to `http://localhost:2525`), and exits non-zero on the
  first failed check. Write each check as an explicit `if … then ok else fail`: a
  `cmd | grep -q x && echo ok` chain reports success for a check that never ran.
- **Synthetic data, no secrets**. Fixtures are written for the template, never copied from a vendor
  account; ids, keys and tokens are obviously fake. Values a test may need to change (magic user ids,
  API keys, validator stamps) are knobs, documented in the manifest.
- **One intercept block per rig**. Only one `--configfile`/`--imposters` source may declare
  `intercept`, so two templates that both carry one cannot be loaded as two sources; merge their
  `intercept.rules` by hand into one file if you need both.

## Manifest (`template.json`)

```jsonc
{
  "name": "optimizely",                 // = the directory name
  "version": "0.1.0",                   // the template's own version
  "summary": "…", "vendor": "Optimizely", "product": "…",
  "entrypoint": "imposters.json",
  "requires": { "rift": ">=0.20.0", "flags": [] },   // the release tarball stamps its own version here
  "ports": { "first": 4600, "last": 4619 },
  "imposters": [{ "port": 4600, "standsInFor": "https://cdn.optimizely.com", "surface": "…" }],
  "fixtures": { … }, "knobs": { … },
  "modes": { "direct": "…", "intercept": "…" },
  "verify": { "blackBox": "smoke.sh", "realSdk": "…" },
  "verifiedAgainst": { "rift": "…", "<sdk>": "…" }
}
```

## Adding a template

1. Create `templates/<name>/` with the layout above and a free 20-port block from 4600–4999.
2. Run it: `rift --configfile templates/<name>/imposters.json`, then `templates/<name>/smoke.sh`.
3. Run the gates: `scripts/verify-templates.sh` (it finds every `templates/*/` on its own), and the
   two cargo tests named above.
4. Add a row to the catalog table here and on `docs/templates/index.md`.
