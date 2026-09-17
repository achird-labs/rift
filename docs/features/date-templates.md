---
layout: default
title: Response Templates
parent: Features
nav_order: 23
---

# Response Templates

Rift has two `{% raw %}{{ }}{% endraw %}` template surfaces for responses, neither of which needs a script engine:

- **Date tokens** (`{% raw %}{{NOW}}{% endraw %}`, `{% raw %}{{DAYS±N}}{% endraw %}`, `{% raw %}{{MONTHS±N}}{% endraw %}`) are always on, in response bodies.
- **`_rift.templated`** is an opt-in function grammar (`{% raw %}{{ request.query.id }}{% endraw %}`, `{% raw %}{{ uuid }}{% endraw %}`,
  `{% raw %}{{ state.hits }}{% endraw %}`, ...) evaluated in the response body **and** every header value.

Both are independent of Mountebank's `${request.*}` substitution, and all of them can appear in the
same response.

---

## Date tokens

| Token | Expands to |
|:------|:-----------|
| `{% raw %}{{NOW}}{% endraw %}` | The current instant. |
| `{% raw %}{{DAYS+N}}{% endraw %}` / `{% raw %}{{DAYS-N}}{% endraw %}` | N days after / before now. |
| `{% raw %}{{MONTHS+N}}{% endraw %}` / `{% raw %}{{MONTHS-N}}{% endraw %}` | N months after / before now. |

Each renders as an **RFC 3339 / ISO 8601 timestamp** in UTC, e.g.
`2026-07-01T16:54:03.803691+00:00`. The tokens are exact: uppercase, no spaces inside the braces.

- They are expanded in every text (non-`binary`) response body, including an imposter's
  `defaultResponse`. A `_rift.templated` response also expands them in header values; otherwise
  headers are left alone.
- An offset that overflows the representable date range leaves the token unchanged rather than
  erroring.

### Example — an issued/expiry token

```json
{
  "port": 4511,
  "protocol": "http",
  "stubs": [{
    "predicates": [{ "equals": { "path": "/token" } }],
    "responses": [{
      "is": {
        "statusCode": 200,
        "headers": { "Content-Type": "application/json" },
        "body": "{\"issued\":\"{% raw %}{{NOW}}{% endraw %}\",\"expires\":\"{% raw %}{{DAYS+30}}{% endraw %}\",\"renews\":\"{% raw %}{{MONTHS+12}}{% endraw %}\"}"
      }
    }]
  }]
}
```

```bash
curl http://localhost:4511/token
# {"issued":"2026-07-01T16:54:03.803691+00:00","expires":"2026-07-31T...","renews":"2027-07-01T..."}
```

---

## `_rift.templated` — the function grammar

Set `"templated": true` in an `is` response's `_rift` block. Without it a literal `{% raw %}{{ ... }}{% endraw %}` (other
than a date token) is served verbatim, so recorded fixtures are never rewritten by accident.

```text
{% raw %}{{ <function> [args...] [| <filter> [args...]] ... }}{% endraw %}
```

Arguments are space-separated words. A single-quoted word (`'like this'`) may contain spaces and is
taken literally; there are no escape sequences. Filters chain left to right.

### Functions

| Function | Result |
|:---------|:-------|
| `request.method` | The request method. |
| `request.path` | The request path. |
| `request.query.<name>` | A query parameter. |
| `request.header '<Name>'` | A request header, matched case-insensitively. For a repeated header, the first value. |
| `request.json '<path>'` | A value from the JSON request body. The path starts with `$` and uses `.key` and `[index]` segments only (`$.items[0].id`). Objects and arrays render as JSON. |
| `now [offset='±N<unit>'] [format='<strftime>']` | The current UTC time. `offset` units are `s`, `m`, `h`, `d`; the default format is RFC 3339. |
| `uuid` | A random UUID v4. |
| `randomInt <a> <b>` | A random integer in `[a, b]`. |
| `state.<key>` | A [flow-state]({{ site.baseurl }}/features/flow-state/) value for the request's flow id (read-only). |
| `previousValue` | Only meaningful inside a [`_rift.stateOps`]({{ site.baseurl }}/features/flow-state/) `set` value; renders empty anywhere else. |

### Filters

| Filter | Result |
|:-------|:-------|
| `\| last_segment` | The last `/`-separated segment (a trailing `/` is ignored). |
| `\| regex '<pattern>' <group>` | Capture group `<group>` of the first match. |
| `\| json` | The value escaped for use **inside** a JSON string literal. Use it whenever a substituted value goes between `"..."`. |

### Example

```json
{
  "port": 4512,
  "protocol": "http",
  "stubs": [{
    "predicates": [{ "startsWith": { "path": "/orders/" } }],
    "responses": [{
      "is": {
        "statusCode": 201,
        "headers": { "Content-Type": "application/json", "X-Request-Id": "{% raw %}{{ uuid }}{% endraw %}" },
        "body": "{\"order\":\"{% raw %}{{ request.path | last_segment | json }}{% endraw %}\",\"sku\":\"{% raw %}{{ request.json '$.items[0].sku' | json }}{% endraw %}\",\"at\":\"{% raw %}{{ now offset='+1h' }}{% endraw %}\"}"
      },
      "_rift": { "templated": true }
    }]
  }]
}
```

### Order and safety

- The `{% raw %}{{ }}{% endraw %}` pass runs on the body and headers as written in the config, **before** `${request.*}`
  substitution. Text that arrives through `${request.*}` is therefore never evaluated, so a client
  cannot inject a template.
- A header value is repaired per substitution: ASCII control characters other than tab (CR, LF,
  NUL, DEL, ...) are removed from what a token substituted, and a `rift::template` warning names
  the stub (`port`, `stub`, `stub_id`) and the removed characters. Non-ASCII text is kept. A control character the author wrote into the header literally still fails the
  response.

### When a token fails

An unknown function or filter, a malformed token, or a failed lookup (missing query parameter,
header, JSON path segment or state key):

- **by default** renders as an empty string and logs a warning on the `rift::template` target;
- **with `RIFT_DEBUG=1`** (or `true`/`yes`/`on`) fails the response with a `500`, the headers
  `x-rift-template-error: true` and `x-rift-imposter: true`, and a JSON error body naming the token.

A flow-store error while reading `state.<key>` follows the same policy.
