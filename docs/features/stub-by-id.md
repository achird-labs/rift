# Stub-by-ID

Stubs can be addressed by a stable `id` instead of by array index, so concurrent edits don't shift
the stub you meant to change.

---

## The `id` field

Give a stub an explicit `id` to address it by id. The id appears in stub listings
(`GET /imposters/{port}/stubs`) and is accepted by the by-id endpoints below. A stub created without
an `id` is addressable by array index only — so **set an `id`** on any stub you intend to manage
by id.

```json
{
  "port": 4545,
  "protocol": "http",
  "stubs": [{
    "id": "greeting",
    "predicates": [{ "equals": { "path": "/hi" } }],
    "responses": [{ "is": { "statusCode": 200, "body": "hi" } }]
  }]
}
```

---

## Endpoints

| Method | Path | Action |
|:-------|:-----|:-------|
| `GET` | `/imposters/{port}/stubs/by-id/{id}` | Fetch the stub |
| `PUT` | `/imposters/{port}/stubs/by-id/{id}` | Replace it **in place** (index preserved) |
| `DELETE` | `/imposters/{port}/stubs/by-id/{id}` | Delete it |

```bash
curl http://localhost:2525/imposters/4545/stubs/by-id/greeting          # 200

curl -X PUT http://localhost:2525/imposters/4545/stubs/by-id/greeting \
  -d '{"id":"greeting","predicates":[{"equals":{"path":"/hi"}}],"responses":[{"is":{"statusCode":200,"body":"hello"}}]}'
curl http://localhost:4545/hi                                           # hello

curl -X DELETE http://localhost:2525/imposters/4545/stubs/by-id/greeting # 200
curl -o /dev/null -w '%{http_code}\n' \
  http://localhost:2525/imposters/4545/stubs/by-id/greeting             # 404
```

A `PUT` by id keeps the stub at its current position in the array; a `DELETE` compacts the list.
