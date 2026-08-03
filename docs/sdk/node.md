---
layout: default
title: Node / TypeScript
parent: Language SDKs
nav_order: 3
permalink: /sdk/node/
---

# rift-node

The official Node.js / TypeScript SDK, published as `@rift-vs/rift`. ESM only, requires Node 20+,
and the core package has zero runtime dependencies.

## Install

```bash
npm install --save-dev @rift-vs/rift
```

`@rift-vs/rift` covers the connect and spawn transports. The in-process engine behind
`rift.embedded()` lives in a second package — installing it is what opts your project into
[koffi](https://koffi.dev/) as a dependency:

```bash
npm install --save-dev @rift-vs/rift-embedded
```

## Hello world

```ts
import { rift, imposter, onGet, onPost, okJson, created, status, times } from '@rift-vs/rift';

await using engine = await rift.embedded(); // or rift.connect(url) / rift.spawn()

const users = await engine.create(
  imposter('users')
    .record()
    .stub(onGet('/api/users/1').willReturn(okJson({ id: 1, name: 'Alice' })))
    .stub(onPost('/api/users').willReturn(created().latency(50), status(503)))); // cycling

// point your system under test at users.url, then:
await users.verify(onGet('/api/users/1'), times(1));
```

## Coming from Mountebank

The Mountebank-compatible `create()` surface is permanent, not a transitional shim:

```javascript
import rift from '@rift-vs/rift';

const server = await rift.create({ port: 2525 });
// ... create imposters, run your tests ...
await server.close();
```

Raw Mountebank imposter JSON round-trips through `fromJson()` and can be mixed with DSL-built
imposters on the same engine, so the typed DSL can be adopted one stub at a time. See also the
[Node.js integration guide]({{ site.baseurl }}/getting-started/nodejs/) on this site.

## Further reading

- [rift-node documentation](https://achird-labs.github.io/rift-node/) ·
  [API reference](https://achird-labs.github.io/rift-node/reference/sdk-api/) ·
  [migration guide](https://achird-labs.github.io/rift-node/mountebank/migration/)
- [rift-node on GitHub](https://github.com/achird-labs/rift-node) ·
  [`@rift-vs/rift` on npm](https://www.npmjs.com/package/@rift-vs/rift)
