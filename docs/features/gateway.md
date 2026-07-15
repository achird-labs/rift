---
layout: default
title: Single-Port Gateway
parent: Features
nav_order: 25
---

# Single-Port Gateway

The gateway dispatches to any imposter through the admin port, so a containerized Rift only needs to
publish **one** port yet still reach every imposter.

---

## Usage

```
/__rift/{port}/{path}
```

Rift extracts `{port}`, rewrites the URI to `/{path}` (query string passed through unchanged), and
serves the request as if it had arrived on the imposter's own port. Any HTTP method works, and the
gateway is **not** gated by `--api-key` (it is data-plane traffic, not the admin control plane).

```bash
# imposter on 4545 has a stub for GET /api/users
curl http://localhost:2525/__rift/4545/api/users?id=1
# identical to:
curl http://localhost:4545/api/users?id=1
```

`/__rift/4545` with no trailing path dispatches to `/`. An unknown port returns `404`; a
non-numeric port returns `400`.

Recorded requests made through the gateway show `requestFrom` as the loopback address, since the
gateway is the imposter's local client.
