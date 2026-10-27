# Behaviors

Behaviors modify responses before they are sent to the client. They enable latency simulation, response transformation, and dynamic content.

---

## Adding Behaviors

Behaviors are added to responses using `_behaviors`:

```json
{
  "is": {
    "statusCode": 200,
    "body": "Hello"
  },
  "_behaviors": {
    "wait": 1000,
    "decorate": "function(request, response) { response.body += ' World'; return response; }"
  }
}
```

`decorate`, `shellTransform` and a `wait` written as a JavaScript function run code, so an imposter
using any of them is refused unless the server was started with `--allowInjection`. A numeric
`wait`, `repeat`, `copy` and `lookup` need no flag. A behavior key set to `null` is treated as
absent everywhere, including by that check.

Behaviors apply to `is` responses (and the flat response form), to `inject` responses, where they run on the response the function returned — a
function that throws runs none of them — and to `proxy` responses, where they run on the upstream's
response before it is recorded (see
[Proxy → Behaviors on the proxy response](proxy.md#behaviors-on-the-proxy-response)),
all as in Mountebank. `repeat` applies to every response type. The other behaviors are not
applied on a `fault` or `_rift`-only response, as in Mountebank. Such a block is not dropped, though: it
is kept and returned by `GET /imposters/:port`, `rift save` and `--datadir` (as Mountebank's
`behaviors` array), a block setting anything besides `repeat` is reported once per response shape
as `config_key_ignored` in `_rift.warnings` and per response by `rift-lint` `W017`, and a scripted
one (`decorate`, `shellTransform`, a function `wait`) still needs `--allowInjection`.

### Alternative Format: behaviors (without underscore)

Some tools generate `behaviors` without the underscore prefix. Both formats are supported:

```json
{
  "is": { "statusCode": 200 },
  "behaviors": {
    "wait": 1000
  }
}
```

### Alternative Format: behaviors as Array

Behaviors can also be specified as an array of behavior objects:

```json
{
  "is": { "statusCode": 200, "body": "Hello" },
  "behaviors": [
    { "wait": 100 },
    { "decorate": "function(request, response) { response.body += ' World'; return response; }" }
  ]
}
```

The array form is a program, as in Mountebank: every element runs, in array order. Two `decorate` elements both run, two `wait` elements both delay, and a `decorate` written before a `copy` runs before it. A `copy`, `lookup` or `shellTransform` holding a list is refused (`400`), as in Mountebank, which reads a list only in the `_behaviors` object form (`rift-lint` `E061`): write one element per item. A `null` for a behavior removes every earlier step of that behavior; an empty list adds nothing and removes nothing. `repeat` is not a step: put it on the response (see [`repeat`](#repeat)). A `repeat` written beside a behavior in an array element, such as `{"wait": 100, "repeat": 3}`, is a custom key and is ignored, as in Mountebank: only the response's `repeat` (or the `_behaviors` object form's) repeats, and its value is not checked (`rift-lint` `W024`). Releases up to 0.23.0 read it as the response's `repeat`. An element that sets several behaviors is refused (`400`), as in Mountebank (`Each behavior object must have only one behavior type`, `rift-lint` `E059`); use one element per behavior, or the `_behaviors` object form. An element with no behavior in it, such as `{"repeat": 2}` or a misspelt key, is refused as `Unrecognized behavior` (`rift-lint` `E060`); a custom key beside one behavior is allowed, as in Mountebank. `_behaviors` runs only when the response sets no `behaviors` (a `"behaviors": null` included) and no top-level `repeat`, as Mountebank upcasts it; beside either it is ignored whole, its `repeat` included, and `rift-lint` reports `W025` (#1709). `"_behaviors": null` counts as absent.

`_behaviors` must be an object; the array form is only accepted under `behaviors`, and each of its elements must be an object (a non-object element is skipped). Any other shape — an array or scalar `_behaviors`, or a scalar `behaviors` — is refused: `POST /imposters` returns `400` and a config file fails to load. This holds with `--allowInjection` on too.

A behavior value the engine cannot read is refused the same way, on any response type and in any array element: for example a `wait` with string bounds (`{"min": "100", "max": "200"}`) or a fractional or negative number, `"repeat": 2.0`, or a `copy` with no `using`. `POST`/`PUT /imposters`, the stub endpoints, `--configfile` and `POST /admin/reload` answer `400` or fail the load naming the key; a `--datadir` file holding one is skipped at startup and listed in the startup error summary. Before 0.18.0 such a block loaded and served without its behaviors. A `copy` or `lookup` whose `using` selector does not compile (a regex, JSONPath or XPath syntax error) is refused the same way; releases up to 0.19.0 loaded it and extracted nothing on every request. `rift-lint` reports these as `E025`, `E035` and `E051`, and checks a regex selector but not a JSONPath or XPath one. Only a step that will run is checked: a `_behaviors` ignored beside `behaviors` or a top-level `repeat` (#1709), or a step a later `null` removes, still loads — except an inverted `wait` range, which is refused wherever it is written.

### How Rift writes behaviors back

`GET /imposters/:port`, `rift save` and `--datadir` always write the array form, in the grammar
Mountebank loads, so a saved file can be posted to Mountebank as is:

```json
{
  "repeat": 3,
  "behaviors": [
    { "wait": 500 },
    { "copy": { "from": "path", "into": "${P}", "using": { "method": "regex", "selector": ".+" } } },
    { "shellTransform": "echo x" }
  ],
  "is": { "statusCode": 200, "body": "Hello ${P}" }
}
```

- `repeat` is written on the response, never as an element — Mountebank and Rift refuse a
  `{"repeat": n}` element. `repeat` must be an integer greater than 0: `0` and `null` are refused,
  as in Mountebank. A `0` stored by an older engine and replayed by an embedder is served as `1`,
  like an absent `repeat`, and is not written.
- Elements come in the order they run: an array's own order, and for the object form `wait`,
  `lookup`, `copy`, `shellTransform`, `decorate`.
- Every step is its own element: a `copy`, `lookup` or `shellTransform` holding several items is
  written one element per item, in order, which is how Mountebank spells it.
- A `null` or empty behavior is not written.

- A `repeat` written beside a behavior in an array element is written back inside its element, as
  Mountebank writes it, and not as the response's `repeat`; it repeats nothing.
- A key that is not a behavior configures nothing. It is written inside the element before it,
  where Mountebank and Rift allow a custom key; with no behavior before it, inside the first one
  after it; with no behavior at all, it is not written.

One shape still does not load in Mountebank: a `wait` written as a `{"min", "max"}` range or
`{"inject": …}` object is a Rift extension.

A pre-0.18.0 `{"repeat": n}` element is refused by `POST /imposters` and `--configfile` in releases
after 0.23.0, as Mountebank refuses it: move it to the response's `repeat`. So is a list inside one
`copy`, `lookup` or `shellTransform` element, which older releases read (and wrote, for `copy`) as one
step per item: write one element per item. A `--datadir` holding one still loads, as does every other shape
an older release stored (see [Data directory](../configuration/cli.md#data-directory---datadir)). The reverse
does not hold either: a `--datadir` written by 0.18.0 and loaded by 0.17.0 or older loses `repeat`,
which those releases do not read on the response.

---

## wait

Add latency to responses. Essential for testing timeout handling.

### Fixed Delay

```json
{
  "_behaviors": {
    "wait": 2000
  }
}
```

Adds exactly 2000ms delay.

### Random Delay

A `{min, max}` range picks a delay uniformly between the two, inclusive (Rift extension). `min`
must not be greater than `max`: an inverted range refuses the imposter at every entry point, and
`rift-lint` reports it as `E025`:

```json
{
  "_behaviors": {
    "wait": { "min": 500, "max": 1500 }
  }
}
```

A JavaScript function can compute the delay instead. The `{"inject": ...}` object spelling is a Rift
extension; Mountebank's own spelling is the bare function string shown in the next section.

```json
{
  "_behaviors": {
    "wait": {
      "inject": "function() { return Math.floor(Math.random() * 1000) + 500; }"
    }
  }
}
```

Returns random delay between 500-1500ms.

### JavaScript Function String

Some tools generate wait as a direct JavaScript function string:

```json
{
  "behaviors": [{
    "wait": " function() { var min = Math.ceil(0); var max = Math.floor(100); return Math.floor(Math.random() * (max - min + 1)) + min; } "
  }]
}
```

### How a wait string is read

A string `wait`, bare or in `{"inject": ...}`, is read the way Mountebank reads it:

1. **`parseInt` first.** When the string starts with a number, that number is the delay and
   nothing is run: `"1000"` waits 1000ms, `"800ms"` waits 800ms.
2. **Otherwise it is called as a function with no arguments**, as `(<string>)()`. Any callable
   expression works: `function () { ... }`, an arrow (`() => 1000`, `x => 300`), `async`, or a
   parenthesised function.

The result is read as Node's `setTimeout` reads a delay: a number is floored, and a result that is
not a number, is negative, or is `NaN` or infinite means no delay. A returned promise is not
awaited, as in Mountebank, so it is no delay, and a timer the function sets never runs. A wait
function has `console` and the timers like any script
([What a script can use](responses.md#what-a-script-can-use)). A wait function that throws,
or does not parse, answers `500` with `invalid wait injection`, as Mountebank does, with or without
`strictBehaviors`:

```json
{
  "errors": [{
    "code": "invalid injection",
    "type": "invalid injection",
    "message": "invalid wait injection",
    "data": "nosuch is not defined"
  }]
}
```

`data` is the thrown error's message, and is left out when the thrown value has none
(`throw 'text'`). The response carries `x-rift-wait-error: true`. A build without the `javascript`
feature cannot run the function: there a body the fallback does not recognise waits 100ms and logs
a warning. A string `wait` needs `--allowInjection` in both engines, even a numeric one.

### Use Cases

**Test client timeouts:**
```json
{
  "stubs": [{
    "predicates": [{ "equals": { "path": "/slow-endpoint" } }],
    "responses": [{
      "is": { "statusCode": 200 },
      "_behaviors": { "wait": 5000 }
    }]
  }]
}
```

**Simulate network latency:**
```json
{
  "_behaviors": {
    "wait": {
      "inject": "function() { return Math.floor(Math.random() * 100) + 50; }"
    }
  }
}
```

---

## decorate

Transform responses using JavaScript. The function is called as `fn(config, response, logger)`,
as in Mountebank, so any function form works: `function (request, response) { ... }`,
`(req, res) => { ... }`, `config => { ... }`, or an `async` function. The parameter names do not
matter: the first argument is `config`, which carries `request`, `response`, `state` and `logger`
and has every request field (`method`, `path`, `headers`, ...) copied onto it, so code written
against `request.path` works. The second argument is the same object as `config.response`. Rift
also passes `state` as a fourth argument.

The function changes the response in one of two ways:

- **Mutate it.** Change `response.statusCode`, `response.headers` or `response.body` and return
  nothing (or anything falsy). The headers sent are exactly the ones `response.headers` holds when
  the function ends, so `delete response.headers['X-Old']` removes that header. A string
  `statusCode` is read as Node reads it (see [Responses](responses.md)), on a `proxy` response too:
  `response.statusCode = "404"` serves and records `404`. A falsy one (`null`, `NaN`, or deleted)
  takes the imposter's `defaultResponse` status, then `200`, as in Mountebank.
- **Return a new one.** A returned object replaces the response; nothing of the original is kept.
  A field it leaves out is taken from the imposter's `defaultResponse`, or is `200`, no headers and
  an empty body when there is none, as in Mountebank. An `async` function's promise is settled
  first and the same rule applies to its value (Mountebank differs when it is falsy; see
  [differences](differences.md#an-async-decorate-that-returns-nothing)). Returning anything
  truthy that is not an object fails the decorate.

While a returned promise is pending, its promise jobs and timers run, so an `async` decorate may
`await` a `setTimeout`. Waiting is bounded by the imposter's script timeout; a decorate still
pending then fails with the decorate timeout. A waiting decorate holds up neither its imposter's
other requests nor any other imposter's scripts, and shares the imposter's `state` object with its
runs in flight, as an inject does ([asynchronous injection](responses.md#asynchronous-injection)).
A decorate that returns no promise is not waited for, as in Mountebank: a timer it sets never runs. Decorate scripts have the same globals as `inject`
(`console`, timers, Promises) and no `process` or Node core modules; see
[What a script can use](responses.md#what-a-script-can-use).

A decorate that is neither a function value nor a bare body using `config.request` or
`config.response` (which Rift wraps in `function (config) { ... }`) runs as a Rhai script; see
[differences](differences.md#decorate-and-a-response-without-headers).

`response.body` arrives as a **string**, even when the stub's `body` is a JSON object, so parse it
before changing fields. If the function sets `response.body` to an object, Rift serializes it back
to JSON. `response.headers` holds one string per header name.

`request` is the same object an `inject` function gets ([Request Object](responses.md#request-object)):
`method`, `path`, `query`, `headers` and `body`, plus `requestFrom` (the client's `address:port`),
`ip` (its address) and `form` (the parsed body, present only for a non-empty
`application/x-www-form-urlencoded` body). On the default bind address an IPv4 client is
`127.0.0.1` in Rift and `::ffff:127.0.0.1` in Mountebank — see
[Client address](differences.md#client-address). A query key, form key or header the client
repeated is an array (`?q=1&q=2` gives `["1", "2"]`), as in Mountebank; one sent once is a string.
The same holds in `MB_REQUEST`. Predicates match each value of such a key. `query` and `form` list their keys in
the order the client sent them, as Mountebank does.

### Basic Transformation

```json
{
  "is": {
    "statusCode": 200,
    "body": { "data": [] }
  },
  "_behaviors": {
    "decorate": "function(request, response) { var body = JSON.parse(response.body); body.timestamp = Date.now(); response.body = body; return response; }"
  }
}
```

### Add Request Info to Response

```json
{
  "_behaviors": {
    "decorate": "function(request, response) { \
      response.headers = response.headers || {}; \
      response.headers['X-Request-Path'] = request.path; \
      response.headers['X-Request-Method'] = request.method; \
      return response; \
    }"
  }
}
```

### Conditional Modification

```json
{
  "_behaviors": {
    "decorate": "function(request, response) { \
      if (request.headers['X-Debug'] === 'true') { \
        response.body = { \
          original: response.body, \
          debug: { path: request.path, query: request.query } \
        }; \
      } \
      return response; \
    }"
  }
}
```

### Parse and Modify JSON

```json
{
  "_behaviors": {
    "decorate": "function(request, response) { \
      var body = typeof response.body === 'string' ? JSON.parse(response.body) : response.body; \
      body.serverTime = new Date().toISOString(); \
      response.body = body; \
      return response; \
    }"
  }
}
```

---

## shellTransform

Run one or more external shell commands that build a new response, as in Mountebank. Each command
receives the request and the current response as JSON and prints the new response, as JSON, to its
standard output. Useful for producing a response with an existing script or CLI tool.

`shellTransform` accepts a single command string, or, in the `_behaviors` object form, an array of
commands that are **chained in sequence**: each command receives the response the previous one
printed. In the `behaviors` array form, write one `shellTransform` element per command, as Mountebank
requires; a list there is refused (`400`).

```json
{
  "is": { "statusCode": 200, "body": "hello" },
  "_behaviors": {
    "shellTransform": "jq -nc '$ARGS.positional[1] | fromjson | .body |= ascii_upcase' --args"
  }
}
```

This serves `HELLO` with status `200`: the command reads the response from its second argument,
upper-cases the body and prints the result.

```json
{
  "_behaviors": {
    "shellTransform": ["./add-header.sh", "./rewrite-body.sh"]
  }
}
```

### What the command receives

Each command runs via `sh -c` and receives the request and the response twice: as two environment
variables, and as two arguments appended to the command line. Rift runs
`<command> '<request JSON>' '<response JSON>'`, as Mountebank does.

| Variable | Argument | JSON shape |
|:---------|:---------|:-----------|
| `MB_REQUEST` | `$1` | `{ "requestFrom", "method", "path", "query", "headers", "body", "ip", "form" }` |
| `MB_RESPONSE` | `$2` | `{ "statusCode", "headers", "body" }` |

Because the arguments are always appended, a command that does not expect them sees two extra
words: `echo hello` prints `hello` followed by both JSON texts. End such a command with ` #` so the
shell treats the arguments as a comment, as Mountebank configurations commonly do:
`printf '{"body":"fixed"}' #`.

When the whole command line reaches 2048 characters (counted as Mountebank counts them, in UTF-16
units), the two arguments are left off and the command runs alone, as in Mountebank. The
environment variables are always set, so a command that reads `MB_REQUEST` and `MB_RESPONSE` works
at any size. Each argument is single-quoted; a `'` inside the JSON is escaped as `'\''`. Mountebank
does not escape it, so there a request or response containing `'` breaks the command line (see
[Differences](differences.md#shelltransform-arguments)).

`MB_RESPONSE.headers` holds one string per header, or an array for a header with several lines
such as `Set-Cookie`. `MB_RESPONSE.body` is the body as written: an `is` body written as JSON (an
object, a number) arrives as that value, as in Mountebank, until a step rewrites the body
(`${request.*}`, `{{ }}`, `copy`, `lookup`, `decorate`), after which it is the body as text. A string
body, a binary one and a proxied one are text. After a command in the same chain printed a JSON body,
the next command receives that value unchanged, so `body += 1` on a number adds, as in Mountebank.

`MB_REQUEST.headers` lists the headers in the order the client sent them, as Mountebank does, and
holds one string per header name: a header the client sent more than once
contributes its **first** value, and a header whose value was not valid UTF-8 is absent from the
object entirely rather than present as `""` (#1040). That is the same view `copy`, `lookup`,
`decorate` and `${request.headers.*}` read, and the same one predicates match on — see
[predicates](predicates.md). `requestFrom` and `ip` are the client's `address:port` and address
(on the default bind address an IPv4 client is `127.0.0.1`, where Mountebank says
`::ffff:127.0.0.1` — see [Client address](differences.md#client-address)). `form` is present only for a non-empty
`application/x-www-form-urlencoded` body, as `JSON.stringify` leaves out Mountebank's undefined one.

### What the command prints

The command's stdout is parsed as JSON, and the result **replaces** the response:

- **A JSON object** is read as an `is` response: `statusCode`, `headers` and `body`. A field the
  object leaves out comes from the imposter's `defaultResponse`, or else is `200`, no headers and an
  empty body. `headers` is taken whole, so a header of the original response that the object does
  not repeat is gone. A non-string `body` is served the way `is` serves it, so `"body": 3` serves
  `3`.
- **Any other JSON value** (a number, a string, an array) replaces the response with an empty one,
  completed the same way.
- **Anything else is a failure**: output that is not JSON (plain text, or nothing at all), the JSON
  `null`, and a non-zero exit. By default the failure is lenient: the response as it was before the
  command is served, with an `x-rift-shelltransform-error: true` header. With `strictBehaviors` /
  `RIFT_STRICT_BEHAVIORS` it returns `500`, whose message is Mountebank's (`Shell command returned
  invalid JSON: '…'`, `Command failed: …`). See [Error Semantics](#error-semantics).

Releases up to 0.23.0 ran the command with no arguments, left `headers` out of `MB_RESPONSE` and
served stdout as the body text. A command written for them that prints a plain body must now print
a JSON response, `{"body": "…"}`.

---

## copy

Copy values from the request to the response. Useful for echoing request data. Tokens are replaced
in the body, in header names and values, and in a `statusCode` string (see
[Responses: Status Codes](responses.md#status-codes)), as in Mountebank. A header whose name changes
replaces any header already of that name (case-insensitively). A token left in a header name (its
source had no value) is not a valid name, so the response is a `500` naming the header, as
Mountebank answers `500` for it.

`from` names the request field: `"path"`, `"method"`, `"body"`, `"ip"` or `"requestFrom"` as a
string, or `{"query": "<name>"}` / `{"headers": "<name>"}` / `{"form": "<name>"}` for one query
parameter, header or urlencoded form field. As in Mountebank, the field name and the name inside the
object are matched case-insensitively (`"BODY"`, `{"query": "Q"}` find `body` and `?q=`), while the
object's own key (`query`, `headers`, `form`) is exact. `using` extracts from that value:

| `method` | `selector` | Result |
|:---------|:-----------|:-------|
| `regex` | A regular expression | The first capture group, or the whole match if the pattern has none. `options` takes `ignoreCase` and `multiline`. |
| `jsonpath` | A JSONPath selector | The first selected value from a JSON source |
| `xpath` | An XPath selector | The first selected value, in document order, from an XML source. An element reads as its first child node, or `undefined` when it has none or starts with an element, as in Mountebank (see [XPath predicates](predicates.md#what-a-selected-node-reads-as)). `ns` maps each namespace prefix the selector uses to its URI. |

A namespaced XML source needs `ns`, as in Mountebank: `"using": { "method": "xpath", "selector":
"//mb:name", "ns": { "mb": "http://example.com/mb" } }`, even when the document declares the
prefix. An XPath selector that fails when it runs on an XML source (a prefix `ns` does not bind, an
unknown function or variable) fails a `copy` with `500` "malformed xpath predicate selector", as in
Mountebank, whether or not the response uses the token. So does a proxy's `predicateGenerators`
`xpath` selector, after the request is forwarded (see
[Generation Failures](proxy.md#generation-failures)). A `lookup` whose key selector fails is
skipped and logged, leaving its tokens in place, as Mountebank does. `ns` must be
an object of strings and is only accepted with `method: xpath`; anything else is refused when the
imposter is loaded (`rift-lint` `E051`), where Mountebank 2.9.5 accepts it. Releases up to
0.21.0 accepted `ns` and ignored it, so a prefixed selector extracted nothing — and an unbound
prefix failed the request without a response.

An XPath selector that evaluates to a string, number or boolean (`string(//a)`, `count(//a)`,
`boolean(//a)`) substitutes that value. Mountebank leaves the token in place for a number or
boolean, and fails the request for `string()`.

Every occurrence of the `into` token in the body and header names and values is replaced. A query
key or header the client repeated is copied from its first value, as in Mountebank. If the source
field is missing, a `regex` selector runs against the text `undefined`, as in Mountebank (so `.*`
copies `undefined`, and `\d+` matches nothing). A `jsonpath` or `xpath` selector extracts nothing
and the token is left in place. When nothing is extracted, the token is left in place, as in
Mountebank. Releases up to 0.21.0 replaced it with an empty string, and up to 0.23.0 a missing
field left the token for every selector. A selector that does not compile is not treated as
"nothing extracted": the imposter is refused when it is loaded.

Rift returns the first capture group where Mountebank returns the whole regex match, so a pattern
like `/users/(\d+)` yields just the id.

**Indexed tokens.** As in Mountebank, `<into>[n]` takes the n-th extracted value: for a `regex`,
`[0]` is the whole match and `[n]` is capture group n; for `jsonpath` and `xpath`, `[n]` is the n-th
selected node, in document order. With `"into": "$C"` and `(\w+)-(\w+)` on `ab-cd`, `$C[1]-$C[2]`
serves `ab-cd` and `$C[0]` serves `ab-cd`. An index with no value, or one not written as Mountebank
writes it (`$C[5]` past the end, `$C[01]`), is not an indexed token, so the bare `$C` inside it is
replaced as usual (`ab[5]`), as in Mountebank. A capture group that did not take part in the match
is replaced with an empty string, where Mountebank inserts a comma. Text a copy inserts is never
scanned for tokens again, so a request value that spells `$C[1]` is served as sent; an authored
`$C[${I}]` whose index an earlier copy inserted is still expanded.

When a `copy` token sits in a **header** value, the substituted text comes from the request, so
Rift removes any character a header value cannot carry (CR, LF, NUL and the other ASCII controls)
and logs a `rift::template` warning naming what it removed, with `port`, `stub` and `stub_id` identifying the stub. A tab and any non-ASCII character are
legal and are kept. Only the substituted text is repaired — a control character you wrote literally
into the header is left alone, and still fails that response with a `500`.

### Copy from Path

```json
{
  "is": {
    "statusCode": 200,
    "body": { "id": "${id}" }
  },
  "_behaviors": {
    "copy": {
      "from": "path",
      "into": "${id}",
      "using": { "method": "regex", "selector": "/users/(\\d+)" }
    }
  }
}
```

Request to `/users/123` returns `{ "id": "123" }`.

### Copy from Query

```json
{
  "is": {
    "statusCode": 200,
    "body": "Page: ${page}"
  },
  "_behaviors": {
    "copy": {
      "from": { "query": "page" },
      "into": "${page}",
      "using": { "method": "regex", "selector": ".+" }
    }
  }
}
```

### Copy from Headers

```json
{
  "is": {
    "statusCode": 200,
    "headers": { "X-Request-Id": "${reqId}" }
  },
  "_behaviors": {
    "copy": {
      "from": { "headers": "X-Request-Id" },
      "into": "${reqId}",
      "using": { "method": "regex", "selector": ".+" }
    }
  }
}
```

### Copy from Body

```json
{
  "is": {
    "statusCode": 200,
    "body": { "received": "${name}" }
  },
  "_behaviors": {
    "copy": {
      "from": "body",
      "into": "${name}",
      "using": { "method": "jsonpath", "selector": "$.user.name" }
    }
  }
}
```

### Multiple Copies

```json
{
  "_behaviors": {
    "copy": [
      {
        "from": "path",
        "into": "${orderId}",
        "using": { "method": "regex", "selector": "/orders/(\\d+)" }
      },
      {
        "from": { "query": "format" },
        "into": "${format}",
        "using": { "method": "regex", "selector": ".+" }
      }
    ]
  }
}
```

---

## lookup

Look up a row in a CSV file, keyed by a value extracted from the request. `key` takes the same
`from` and `using` as [`copy`](#copy), so `key.from` matches field names case-insensitively too. Each column of the matched row replaces the token
`<into>[<column>]`, so with `"into": "${row}"` the `email` column fills `${row}[email]`. As in
Mountebank, the column may also be quoted: `${row}["email"]` and `${row}['email']` are replaced too.
Tokens are replaced in the body, in header names and values and in a `statusCode` string, as with
`copy`. A key field the request does not carry is read as `undefined` by a `regex`, as in
Mountebank, so `.*` looks up the row keyed `undefined`.

`key.index` picks which match keys the row, as in Mountebank. For a `regex`, `0` is the whole
match and `n` is the `n`th capture group; for `jsonpath` and `xpath`, `n` is the `n`th selected
value, counting from `0`. An index past the last match finds no row. Without `index` the key is what
[`copy`](#copy) extracts: the first capture group (the whole match when the pattern has none), or the
first selected value. Mountebank's default is index `0`, the whole match, so a Mountebank config
that relies on that default with a capture-group pattern should say `"index": 0`. Any value is
accepted and written back as written, and read as Mountebank reads `keyValues[index || 0]`: `0`,
`""` and `false` are the first match, a non-negative integer or a numeric string such as `"1"`
picks that match, and anything else (`-1`, `1.5`, `"01"`, `true`) finds no row. Releases up to
0.23.0 refused an index that was not a non-negative integer. The same `index` is kept on a
`_rift.dataset` block's `key`.

As with [`copy`](#copy), a `lookup` token in a **header** value is repaired after substitution: the
request chooses which row is read, so a CSV cell holding a character a header value cannot carry
would otherwise fail the whole response. Those characters are removed from the substituted cell and
a `rift::template` warning names them, alongside the `port`, `stub` and `stub_id` of the stub that produced it. Literal text you wrote around the token is left alone.

### CSV Lookup

```json
{
  "is": {
    "statusCode": 200,
    "body": { "name": "${row}[name]", "email": "${row}[email]" }
  },
  "_behaviors": {
    "lookup": {
      "key": {
        "from": "path",
        "using": { "method": "regex", "selector": "/users/(\\d+)" }
      },
      "fromDataSource": {
        "csv": {
          "path": "users.csv",
          "keyColumn": "id"
        }
      },
      "into": "${row}"
    }
  }
}
```

With `users.csv`:
```csv
id,name,email
1,Alice,alice@example.com
2,Bob,bob@example.com
```

Request to `/users/1` returns `{ "name": "Alice", "email": "alice@example.com" }`.

When several rows match, the first one in the file is used, as in Mountebank. Releases up to
0.21.0 served the last row for a repeated value in the first column, and an arbitrary one for a
repeated value in any other column.

`csv.delimiter` sets the field separator (default `,`). It may be several characters, such as
`||`, and an empty string means `,`. The file is cached per delimiter, and an edited file is read
again on its next use, as Mountebank, which reads the file on every lookup, sees the edit. If the
file cannot be read, a warning is logged and the tokens are left in place.

Reading a file draws on `--body-memory-budget`: while the budget is used up, the request gets
`503 Service Unavailable` with `Retry-After: 1`, with or without `strictBehaviors`. A file larger
than `--max-body-size` is not read: a warning is logged and the tokens are left in place. The cache
holds at most `--max-body-size` of parsed files and drops the least recently used past it, so a
dropped file is read again on its next use. A file whose parse alone is larger than that is not
cached: a warning is logged once, the file is read again on every use, and its parse draws on
`--body-memory-budget` until the response is built (#1654). Concurrent requests that miss on the
same file share one read and parse, and its outcome (#1715): a cold or just-edited file draws on the
budget once, not once per request. Only a regular file is read, from any door: a FIFO or a device is
refused with a warning.

Which files a lookup may read depends on how its imposter was written (#1617), unlike Mountebank,
which reads any path:

| The imposter came from | `path` may name |
|:--|:--|
| The admin API, a `--datadir` file, or an `https:` source, with `--scripts-dir` | Only files under `--scripts-dir`; a relative path from there. |
| The same, without `--scripts-dir` | Nothing. |
| A local `--configfile` (or the embedding C API) | Any file; a relative path from the server's working directory. |

So `users.csv` above, posted over the admin API, is `<scripts-dir>/users.csv`; start the server
with `--scripts-dir` pointing at the directory that holds your data files. The admin API refuses
a path outside the root (absolute, `..`, or a link that points out) with `400`: `lookup CSV path
'<path>' is outside --scripts-dir`, or, without `--scripts-dir`, `lookup CSV path '<path>' cannot
be read: no --scripts-dir is configured`. The answer is the same whether or not anything exists
there. A file inside the root that does not exist yet is accepted, and read once it does. The path
is resolved again each time the lookup runs, so a link changed to point outside the root later is
refused then. See [Differences](differences.md#lookup-reads-csv-files-only-under---scripts-dir).

The file is parsed as Mountebank's parser (csv-parse) parses it:

- A cell in double quotes may hold the delimiter and line breaks, and `""` inside it is one `"`.
- Nothing is trimmed: ` Bob ` is not `Bob`, and a header written `name, location` names the
  column ` location`, so `${row}[location]` is left in place.
- The first line break in the file (`\n`, `\r\n` or `\r`) is the one that ends rows.
- Every row must have as many cells as the header row; a blank line is a row of one empty cell.
- A malformed row — a different cell count, a `"` inside an unquoted cell, text after a closing
  quote, or a quote left open — ends the file at that row. Rows before it are served; rows after
  it are not found, and a warning names the file and the line. In Mountebank the rows after it
  do not exist either.

Releases up to 0.23.0 split rows on line breaks and cells on a single-character delimiter, with no
quoting, and trimmed every cell and header.

---

## repeat

Control how many times a response is returned before cycling to the next response.

### Basic Repeat

```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "First response" },
      "_behaviors": { "repeat": 3 }
    },
    {
      "is": { "statusCode": 200, "body": "Second response" }
    }
  ]
}
```

The first response is returned 3 times before advancing to the second:

- Requests 1-3 → "First response"
- Request 4 → "Second response"
- Requests 5-7 → "First response" (cycles back)
- Request 8 → "Second response"

`repeat` works on every response type — `is`, `inject`, `proxy`, `fault` and `_rift`-only.

In the `behaviors` array form, `repeat` goes on the response, not in an element: one beside a
behavior in an element (`"behaviors": [{"wait": 100, "repeat": 3}]`) is ignored, as in Mountebank.

Mountebank itself stores `repeat` on the response rather than in the block, and that is the form
`mb save` writes; Rift reads it too:

```json
{ "is": { "statusCode": 200, "body": "First response" }, "repeat": 3 }
```

It is held to the same rule as the block's — `repeat` must be an integer greater than 0 and within
32 bits, so `0`, `null`, `-1`, `"2"` and `2.5` refuse the imposter, and `rift-lint` reports it as
`E035`. When both are written, a top-level `repeat` wins over one in a `behaviors` block, and
beside `_behaviors` it keeps the whole block from running, as in Mountebank (#1709). Mountebank refuses `0`
and `null` too, but accepts a numeric string or a fraction (see
[Differences](differences.md#repeat-is-a-whole-number)). Rift writes it back the same way — on the response, not
in the `behaviors` array — see [How Rift writes behaviors back](#how-rift-writes-behaviors-back).

### Per-Response Repeat

Each response can have its own repeat count:

```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "Success" },
      "_behaviors": { "repeat": 5 }
    },
    {
      "is": { "statusCode": 500, "body": "Error" },
      "_behaviors": { "repeat": 2 }
    }
  ]
}
```

Returns "Success" 5 times, then "Error" 2 times, then cycles.

### Use Cases {#use-cases-1}

**Simulating rate limiting:**
```json
{
  "responses": [
    {
      "is": { "statusCode": 200, "body": "OK" },
      "_behaviors": { "repeat": 10 }
    },
    {
      "is": {
        "statusCode": 429,
        "headers": { "Retry-After": "60" },
        "body": "Rate limited"
      }
    }
  ]
}
```

Allows 10 requests, then returns 429, then cycles.

**Simulating quota exhaustion:**
```json
{
  "responses": [
    {
      "is": { "body": { "remaining": 100 } },
      "_behaviors": { "repeat": 50 }
    },
    {
      "is": { "body": { "remaining": 50 } },
      "_behaviors": { "repeat": 50 }
    },
    {
      "is": { "statusCode": 403, "body": "Quota exceeded" }
    }
  ]
}
```

**Testing retry logic with eventual success:**
```json
{
  "responses": [
    {
      "is": { "statusCode": 503, "body": "Service unavailable" },
      "_behaviors": { "repeat": 2 }
    },
    {
      "is": { "statusCode": 200, "body": "Success after retries" }
    }
  ]
}
```

Fails twice, then succeeds - perfect for testing retry mechanisms.

### Without Repeat

Responses without `repeat` default to 1 - they are returned once before advancing:

```json
{
  "responses": [
    { "is": { "body": "First" } },
    { "is": { "body": "Second" } },
    { "is": { "body": "Third" } }
  ]
}
```

Each response is returned once in sequence (standard cycling).

---

## Behavior Order

In the array form, behaviors run in array order. In the object form (`_behaviors`), and within an
array element that sets several, they run in Mountebank's order:

1. **wait** - Delay first
2. **lookup** - Perform data lookups
3. **copy** - Copy request values into response
4. **shellTransform** - Pipe the body through each command in turn
5. **decorate** - Transform the response last

Earlier Rift releases ran copy before lookup and decorate before shellTransform.

To run them in another order, use the array form with one behavior per element. `repeat` is not a
step here; it controls which response is chosen.

### Substituted text is never re-scanned

Text the engine substitutes into a response is never read as a token again: a token is expanded
only if you wrote its first and last characters. This covers the `${request.…}` and
`_rift.templated` `{{ }}` passes, which run before behaviors, and `copy` and
`lookup`, in body and header values alike. A client that sends `${R}[secret]` in a query parameter
therefore gets `${R}[secret]` back, never the `secret` column of the row the lookup matched. Sending
only part of a token does not help either. `${R}[secret` followed by a `]` you wrote, or
`{R}[secret]` after a `$` you wrote, stays as sent. The same holds for a CSV cell that happens to
contain another column's token.

Only the middle of a token may be substituted. A token you split around a copy, such as
`${R}[${COL}]` with a copy into `${COL}` followed by a lookup into `${R}`, is still expanded,
because `${R}[` and `]` are yours. That is how to let the client choose the column when you mean
to.

Three sources count as your text, because they are configuration rather than request data:

- **The output of a `decorate` or `shellTransform`.** The script can rewrite the response in any
  way, so Rift cannot tell what came from the request. In `[{copy}, {decorate}, {lookup}]`, the
  lookup expands whatever the decorate returned, including copied text.
- **A proxied response body.** A `lookup` on a proxy response expands the tokens the upstream
  returns, since the upstream is the one named in `proxy.to`.
- **What an `inject` function returns.** Its behaviors treat the whole response as yours.

The serve-time date tokens (`{{NOW}}`, `{{DAYS+N}}`,
`{{MONTHS+N}}`) are the one pass that still reads the finished body, so a
client that sends `{{NOW}}` gets a timestamp back. They expand only to a date.

**Differs from Mountebank:** for `"behaviors": [{"copy": …}, {"lookup": …}]` Mountebank 2.9.1 expands
a lookup token the client put in the copied field. Rift serves it as sent.

---

## Combining Behaviors

```json
{
  "is": {
    "statusCode": 200,
    "body": { "userId": "${id}", "processed": false }
  },
  "_behaviors": {
    "copy": {
      "from": "path",
      "into": "${id}",
      "using": { "method": "regex", "selector": "/users/(\\d+)" }
    },
    "decorate": "function(request, response) { var body = JSON.parse(response.body); body.processed = true; body.timestamp = Date.now(); response.body = body; return response; }",
    "wait": 100
  }
}
```

---

## Error Semantics

When a transforming behavior fails — a `decorate` function throws, a `shellTransform` command exits
non-zero, or a `binary`/base64 body can't be decoded — Rift signals the failure rather than failing
silently.

**Default (lenient).** The fallback response (the un-transformed body) is still served with its
normal status, and a header flags what failed:

| Behavior | Failure header |
|:---------|:---------------|
| `decorate` | `x-rift-decorate-error: true` |
| `shellTransform` | `x-rift-shelltransform-error: true` |
| `binary` (base64) — an `is` body **or** a `defaultResponse` body | `x-rift-binary-error: true` |

**Strict mode.** Set the per-imposter `strictBehaviors` flag (or the `RIFT_STRICT_BEHAVIORS`
environment variable, truthy: `1`/`true`/`yes`/`on`) to turn a behavior failure into a
`500 Internal Server Error` — the failing behavior no longer serves a fallback. The response still
carries the matching `x-rift-<behavior>-error` header. The per-imposter flag and the env var combine
with **OR**: either being set forces strict mode. The default is lenient (both unset).

```json
{
  "port": 4545,
  "protocol": "http",
  "strictBehaviors": true,
  "stubs": [{
    "responses": [{
      "is": { "statusCode": 200, "body": "Hello" },
      "_behaviors": {
        "decorate": "function(request, response) { throw new Error('boom'); }"
      }
    }]
  }]
}
```

With `strictBehaviors` on, the throwing `decorate` above returns `500` instead of serving `"Hello"`.

---

## Best Practices

1. **Use wait sparingly** - Only for testing timeout handling
2. **Keep decorate functions simple** - Complex logic is hard to debug
3. **Use copy for echoing** - More maintainable than decorate for simple cases
4. **Test behaviors individually** - Easier to debug
5. **Document behavior purpose** - Future maintainers will thank you
