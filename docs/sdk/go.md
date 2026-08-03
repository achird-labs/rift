---
layout: default
title: Go
parent: Language SDKs
nav_order: 4
permalink: /sdk/go/
---

# rift-go

The official Go SDK. Requires Go 1.24+, and the embedded engine loads through
[purego](https://github.com/ebitengine/purego) rather than cgo — so `CGO_ENABLED=0` keeps working,
no C toolchain is needed, and cross-compilation is unaffected.

## Install

```bash
go get github.com/achird-labs/rift-go
go run github.com/achird-labs/rift-go/cmd/rift-fetch@latest -version v0.17.0
```

The second line downloads and SHA-256-verifies the native library for the embedded transport; the
connect and spawn transports do not need it.

## Hello world

```go
func TestUserLookup(t *testing.T) {
	users := rifttest.Imposter(t, rift.NewImposter("users").
		Stub(rift.OnGet("/api/users/1").
			Return(rift.OKJSON(map[string]rift.JSON{"id": 1, "name": "Alice"}))))

	callSUT(t, users.BaseURL())

	rifttest.AssertReceived(t, users, rift.OnGet("/api/users/1"), rift.Once())
}
```

`rifttest` owns the lifecycle: a shared engine, `t.Cleanup` teardown, and request assertions that
report the nearest non-matching request when they fail.

## Transports

All three implement `rift.Client`, so a suite written against one runs against the others:

```go
// In-process. No binary, no port, no cleanup.
eng, err := riftembed.Start(riftembed.Options{})

// An engine already running somewhere.
eng, err := rift.Connect("http://localhost:2525", rift.RemoteOptions{})

// A binary this process manages. ctx bounds startup; Close stops it.
eng, err := rift.Spawn(ctx, rift.SpawnOptions{})
```

## Further reading

- [rift-go documentation](https://achird-labs.github.io/rift-go/) — package map, intercept, and the
  `testing.T` helpers
- [rift-go on GitHub](https://github.com/achird-labs/rift-go)
