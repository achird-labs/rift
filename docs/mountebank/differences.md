# Differences from Mountebank

Rift answers the Mountebank API the way Mountebank 2.9.5 does, with the deliberate exceptions on
this page. Every one of them is pinned by the
[differential harness](https://github.com/achird-labs/rift/tree/master/conformance/differential): it
runs each test case on a real Mountebank and on Rift, and fails on any difference that its allow-list
does not cite from these docs or name as a known bug. A behaviour that differs and is *not* listed
here is a bug.

## Marker headers on served responses

Every response an imposter serves carries `x-rift-imposter: true`. Mountebank sends only the headers
the response configures. Some responses carry one more marker:

| Header | When |
|:-------|:-----|
| `x-rift-proxy: true` | The response came from a `proxy` |
| `x-rift-proxy-latency: <ms>` | A proxied response, with the upstream's time in milliseconds |
| `x-rift-proxy-error: true` | The proxy could not reach the upstream (see [Proxy](proxy.md)) |
| `x-rift-wait-error: true` | A `wait` function threw; the body is Mountebank's `invalid wait injection` error (see [Behaviors](behaviors.md#how-a-wait-string-is-read)) |
| `x-rift-inject-error: true` | A response or predicate `inject` threw; the body is Mountebank's `invalid injection` error (a throw is `500` with the thrown message in `data`) |
| `x-rift-inject: true` | An `inject` response produced it |
| `x-rift-default-response: true` | No stub matched and the imposter's `defaultResponse` answered |
| `x-rift-no-match: true` | No stub matched and the imposter has no `defaultResponse` |

A proxy whose upstream is itself a Rift imposter therefore records that imposter's markers in the
saved response's `headers`, like any other upstream header.

## The admin API serves JSON only

Rift's admin API has no HTML views, so it does not negotiate on `Accept`: it sends no `Vary: Accept`
header.

## Error envelopes

Error envelopes carry `code` (the HTTP status), `type` and `message` — see the
[API reference](../api/index.md) — where Mountebank's carry `code` (Rift's `type`) and `message`. Rift
does not add Mountebank's `source`, `name` or `stack` fields, and its messages are worded
differently. Nor does it add `inner`, the error Mountebank wraps in a `bad data` one. A throwing
predicate `inject` carries `code` `invalid predicate injection`, where Mountebank says
`invalid injection`.

When an imposter's address is held by another process, both engines answer `400`; Mountebank's body
is node's raw `EADDRINUSE` object (`code`, `errno`, `syscall`, `address`, `port`, no `message`), Rift's
is its own envelope.

## Request header names

Rift cannot see the spelling a client used for a header name: an imposter sees names Title-Cased, and an inject or decorate script finds a header under any spelling. Mountebank passes scripts the names exactly as the client wrote them, so
`request.headers['content-type']` is `undefined` there when the client sent `Content-Type`, and
the reverse. In Rift both spellings find the header; `Object.keys(request.headers)` and
`JSON.stringify(request.headers)` show the Title-Cased names (`Content-Type`), in the order the client sent them. The same root cause
shows in two places that are not scripts: a header-name predicate with `caseSensitive: true` (or
`keyCaseSensitive: true`) compares against the Title-Cased name, and `savedRequests` record the
Title-Cased name.

## Response header names

Over HTTP/1.1 Rift writes response header names Title-Case. Mountebank writes a configured name
exactly as spelled and forwards a proxied origin's own spelling. So a stub's `lower-hdr` arrives as
`Lower-Hdr`, and a proxied `X-Mixed-CASE` arrives as `X-Mixed-Case`. Header names are
case-insensitive, so no conforming client is affected; hyper has no public API to write a name's
original spelling. Over HTTP/2 the names are lower-case in both engines.

## Client address

An imposter with no `host` listens dual-stack, as in Mountebank, so both engines report an IPv4
client as `::ffff:127.0.0.1`. `requestFrom` is spelled differently: Mountebank writes
`::ffff:127.0.0.1:<port>`, Rift `[::ffff:127.0.0.1]:<port>`. An imposter bound to
`"host": "0.0.0.0"` listens on IPv4 only, in both engines, and Rift then reports the client as
`127.0.0.1`.

Each engine uses its own value everywhere: the `ip` and `requestFrom` predicates, generated predicates ([Proxy](proxy.md#predicate-generators)) and the `request.ip` /
`request.requestFrom` that `inject`, `decorate` and `shellTransform`'s `MB_REQUEST` see. Rift does
not rewrite an address into the `::ffff:` form, because a script would then see a value its own
`ip` predicate never matches. An IPv6 client's `requestFrom` is written `[::1]:50000` in Rift and
`::1:50000` in Mountebank.

A recorded request (`requests[]`, the journal) carries both `requestFrom` and `ip`, as Mountebank's
does; `ip` is the same value the `ip` predicate sees.

## `decorate` and a response without headers

`decorate` always receives a `headers` object on the response, empty when the response has none; in Mountebank `response.headers` is `undefined` there, so a decorate that writes a header fails. Rift accepts that script, so `config.response.headers['X-D'] = 'y'` on an `is` with no headers
adds the header. More generally, in Mountebank 2.9.1 any decorate that throws answers `500` with a
JavaScript stack trace in the body (its error handler fails while logging the request). Rift serves
the undecorated response with `x-rift-decorate-error: true` instead, and `500` only under
`strictBehaviors` — see [Behaviors](behaviors.md). A decorate that is not a function at all (`42`,
a bare expression) answers `500` in Mountebank; Rift runs it as a Rhai script, so it may succeed.

## `require()` loads files only, from `--scripts-dir`

Mountebank's `require` is Node's: it loads built-ins such as `fs` and any file on the host. Rift's
loads only file modules (CommonJS `.js`, and `.json`), with no built-ins and no package lookup. A
script in an imposter written over the admin API, from `--datadir` or from an `https:` source may
load files only from under `--scripts-dir`, and none without it; a local `--configfile` imposter may
load any path. A path outside the root is refused with `require('<path>'): outside --scripts-dir`.
Rhai `import` is not supported, and neither is JavaScript dynamic `import()`, which Node resolves and Rift rejects. See [Scripting](../features/scripting.md#files-a-script-may-read).

## `lookup` reads CSV files only under `--scripts-dir`

Mountebank's `lookup` reads `fromDataSource.csv.path` wherever it points, with or without
`--allowInjection`, so any admin client can read a host file whose lines look like CSV rows. Rift
confines it like `require()`: an imposter written over the admin API, from `--datadir` or from an
`https:` source reads CSV files only under `--scripts-dir`, a relative path from there (not from
the working directory), and none without it. A local `--configfile` imposter reads any path, a
relative one from the working directory, as Mountebank does.

The admin API refuses a path outside the root (absolute, `..`, or a link that points out) with
`400` and `lookup CSV path '<path>' is outside --scripts-dir`, and any path without `--scripts-dir`
with `lookup CSV path '<path>' cannot be read: no --scripts-dir is configured`, whether or not
anything exists there. A file that does not exist yet inside the root is accepted. A stored
`--datadir` file is not refused for it; the path is resolved again every time the lookup runs, and
one outside the root leaves the tokens in place with a warning (#1617). See
[Behaviors](behaviors.md#csv-lookup).

## Promise jobs a hook leaves behind are dropped

Mountebank runs every injection in one Node process, so a `.then` callback or a timer a script
queues runs whenever it comes due, after the request was answered, and globals are shared by every
imposter. Rift runs a hook's promise jobs and timers only while it waits for that hook's answer (an
`inject` until it has one, an `async` `decorate` until its promise settles), within the script
timeout, and drops what is left when the hook ends. It shares globals only between scripts with the
same `require()` access, so a script written over the admin API never runs code inside a
`--configfile` imposter's hook (#1645).

## Scripts run in an ECMAScript sandbox, not Node

Mountebank evaluates `inject`, `decorate` and `wait` functions inside its own Node process, so a
script sees Node's whole global scope. Rift runs them in its own JavaScript engine, with standard
JavaScript plus `console`, Promises (`async`/`await`) and the timers (`setTimeout`,
`setInterval` and their `clear` functions); see
[What a script can use](responses.md#what-a-script-can-use).

There is no `process`, so no `process.env`: on a shared node (rift-cluster) the environment would
hand every tenant's script the node's secrets. There is no `Buffer` and no Node core module
(`require('http')`, `require('fs')`); `require` loads a CommonJS or JSON file only
([above](#require-loads-files-only-from---scripts-dir)). A script that needs the network or the
environment should be a `proxy`, a `shellTransform`, or an `inject` fed by state (#1593).

## `shellTransform` arguments

`shellTransform` appends the request and response JSON to the command as two single-quoted
arguments, as Mountebank does. Rift escapes a `'` inside the JSON as `'\''`, so the command still
receives both arguments whole; Mountebank does not escape it, so a request or response containing
`'` breaks its command line. Rift's JSON keys may come in a different order. Where Mountebank's
command line works, each argument is the same JSON. See [Behaviors](behaviors.md#shelltransform).
`MB_RESPONSE.body` is the body as text, where Mountebank hands an `is` body written as JSON (an
object, a number) over as that value; a body a previous command in the chain printed keeps its JSON
type in both.

A command that fails, or prints anything other than JSON, is a `500` in Mountebank. Rift serves the
response as it was before the command, with `x-rift-shelltransform-error: true`, and answers `500`
only under `strictBehaviors`, as for `decorate`.

## `copy` or `lookup` from a whole `query` or `headers` object

A `copy` or `lookup` whose `from` is the string `"query"` or `"headers"` (the whole object, not one
key of it) extracts nothing in Rift, so its token is left in place. Mountebank reads the object as
the text `[object Object]`, or fails the request with a `500` `TypeError` for `query`. A missing
field is read as `undefined` in both, as Mountebank does — see [Behaviors](behaviors.md#copy).

## `copy` indexed tokens

`<into>[n]` takes the n-th extracted value, as in Mountebank. Two cases differ: a regex capture
group that did not take part in the match is replaced with an empty string (Mountebank inserts a
comma), and inserted text is never re-scanned for tokens — see [Behaviors](behaviors.md#copy).

## Behavior validation stops at the first error

Rift refuses a behaviors block with the same rules as Mountebank, but names only the first problem:
where Mountebank lists every one (a `copy` missing `from`, `into` and `using` is three errors
there), Rift's `400` names the first.

## `repeat` is a whole number

`repeat` must be a whole number from 1 to 4294967295, written as a number. Mountebank also accepts
a numeric string (`"2"`, `" 2"`) or a fraction (`1.5` repeats twice), and a string that is not a
number (`"abc"`), which it then serves zero times. Rift refuses each of these when the imposter is
created.

## `lookup` without `key.index`

Without `key.index`, a lookup keys the row by the first capture group (the whole match when the
pattern has none). Mountebank's default is index `0`, the whole match. `"key.index": null` is the
same as leaving it out. See [Behaviors](behaviors.md#lookup).

## `exists` with an object over an array

`{"exists": {"body": {"items": {"id": true}}}}` against `{"items": [...]}` holds in Mountebank for
any array, whatever its elements and whatever the expectation asks: its array shortcut fires before
the elements are looked at. Rift holds it when some element satisfies the object, so
`{"items": [{"sku": 1}]}` does not satisfy `{"id": true}`. See [Predicates](predicates.md#exists).

## `except` on a JSON body

Rift applies `except` to each value of a JSON body after parsing it. Mountebank also applies it to
the raw body text before parsing, so a pattern that matches JSON punctuation (a quote, a brace) can
make the body unparseable there, so every field in it reads as missing.

## An undecodable percent escape stays as sent

In a query string or form body, Mountebank turns an invalid UTF-8 escape such as `%FF` into U+FFFD. Rift keeps the text as sent (`%FF`), so a predicate can still match it. Other `+`/`%` decoding is the same: `+` is a space and `%2B` is a plus.

## Scripts and templates see a repeated key comma-joined

Predicates match each value of a repeated query or form key, as Mountebank does (see
[Repeated keys](predicates.md#repeated-keys)). An `inject` function, `decorate`, `shellTransform`'s
`MB_REQUEST`, `_rift.script` and `${request.query.*}` template substitution still receive one string
per key, the values comma-joined in order (`?q=1&q=2` is `"1,2"`), where Mountebank gives a script
the array `["1", "2"]`.

## An array expectation against a single value

An array expectation is not matched against a single string by its characters:
`{"equals": {"query": {"key": ["f"]}}}` does not match `?key=first`, and
`{"equals": {"body": {"t": ["a"]}}}` does not match `{"t": "abc"}` or `{"t": "a"}`. Mountebank
reads the array's indexes as fields of the string, so `["f"]` matches there because the first
character of `first` is `f`. A value that is a JSON array (a body, a body field holding one, or
`?key=["f"]`) is still compared as one. See [Arrays in a JSON body](predicates.md#arrays-in-a-json-body).

## `contains` against a JSON `null`

`{"contains": {"body": {"a": "x"}}}` against `{"a": null}` does not match in Rift. Mountebank
answers `500` (`TypeError: Cannot read properties of null`), as it does for `startsWith` and
`endsWith`; a `null` has no text to search.

## `except` with an `xpath` or `jsonpath` selector

With an `xpath` or `jsonpath` selector, Rift applies `except` to each selected value, after selecting.
Mountebank applies it to the whole body before selecting, so the selector then reads the stripped
text. An `except` that only removes noise from a value, such as `\d+`, behaves the same in both. One
that alters the document's structure (tags, quotes, key names) can stop a Mountebank match that Rift
still makes, or the reverse. See [Predicates](predicates.md#except).

- An XPath `except` that removes markup still matches in Rift. In Mountebank it never matches: `{"equals": {"body": "1"}, "xpath": {"selector": "//b"}, "except": "<a>"}` strips `<a>` from `<a><b>1</b></a>` first, leaving `<b>1</b></a>`, which is no longer well-formed.
- A JSONPath `except` that removes quotes still matches in Rift. In Mountebank it never matches: `{"equals": {"body": "1"}, "jsonpath": {"selector": "$.a"}, "except": "\""}` strips the quotes from `{"a":1}` first, leaving `{a:1}`, which is not JSON.

The [JSON body](#except-on-a-json-body) entry covers the same difference for a predicate with no
selector.

## Binary mode refuses `matches` wherever it sits

A `"mode": "binary"` imposter refuses a `matches` predicate or a `jsonpath`/`xpath` selector with
`400`, as Mountebank does, but Rift refuses one anywhere under `and`/`or`. Mountebank raises the
refusal from a dry run of each stub against a synthetic request, and `and`/`or` stop early there: an
`or` whose earlier branch matches the dry-run request (`{"equals": {"path": "/"}}`), or an `and` whose
earlier branch does not, never reaches the later `matches`, so Mountebank creates that imposter. The
stub could never use the `matches` on a binary body either way, so Rift refuses it as written rather
than depend on Mountebank's synthetic request.

## Body size

Mountebank has no body limit and crashes past about 512 MiB, where a body no longer fits a V8
string. Rift refuses a body over `--max-body-size` (default 512 MiB) with `413`, and answers `503`
with `Retry-After` when the bodies buffered at once would exceed `--body-memory-budget` (default
1 GiB), where Mountebank would run out of heap. Below those limits the two serve the same bodies.
See [Body size limits](../configuration/cli.md#body-size-limits).

## Recorded requests are bounded by bytes

Mountebank's request journal grows without limit. Rift keeps each imposter's recorded requests
under a byte budget, `--journal-max-bytes` (default 768 MiB), evicting the oldest first; there is
no count limit, so small requests are kept in any number. The default holds any body the body
ceiling admits, even a binary one, which is journalled base64-encoded (a third larger). A body
larger than the whole budget (possible only with a budget set lower) is recorded without its body
and flagged `"_bodyOmitted": true`, never dropped.
A cursor read past evicted entries reports it with `x-rift-truncated`.
With `--datadir`, the same budget bounds what a restart restores from `<port>.requests.jsonl`.
Mountebank writes each recorded request to its datadir before answering it, so a crash loses none;
Rift writes them behind, about once a second and on a graceful stop, so a crash can lose the last
~2 seconds of recorded requests.

## `--debug`

Mountebank's `--debug` lists every match on each stub of `GET /imposters/:id` (`matches`). Rift's
`--debug` raises the log level and enables script tracing; it does not record matches per stub, and
`GET /imposters/:id` never has a `matches` key. To see which stub answered each request, set
`recordRequests: true`: each recorded request carries `matchOutcome.stubIndex` (the matched stub),
`status` and `latencyMs`. To ask which stub *would* answer, send the request with
`X-Rift-Debug: true` (see [Debug mode](../features/debug-mode.md)).

## Malformed XML for XPath

Rift reads an XML body for `xpath` the way Mountebank's parser, xmldom 0.9, recovers from it (see
[Predicates](predicates.md#xpath-predicates)), except in these cases:

- A `<` followed by a letter that does not parse as a tag (`<a>x <b</a>`, `if (i<n)`) means no
  document in Rift. Mountebank recovers it as text; Rift does not copy that, because reading a tag
  that fails to parse as text would also turn a truncated or mismatched tag into a match.
- After a stray end tag naming the root, any further tag (`<a></a></a><b/>`) means no document in
  Rift. Mountebank yields the root there by surviving an internal TypeError, not by a rule; Rift
  ignores only the stray end tag followed by text, comments and processing instructions.
- A character reference to a lone surrogate or past U+10FFFF (`&#xD800;`, `&#99999999999;`) reads
  as U+FFFD in Rift. Mountebank yields a lone surrogate, or garbage for an out-of-range number, which
  a Rust string cannot hold; `exists` and `matches` agree, and an `equals` cannot target
  Mountebank's value.
- A `]` inside a quoted value in a DOCTYPE's internal subset (`<!DOCTYPE a [<!ENTITY x "]">]>`)
  means no document in Rift, whose parser ends the subset there. Mountebank reads past it.

## Case-insensitive XPath

With `caseSensitive` false (the default), Mountebank lowercases the whole body and the selector
before an XPath predicate selects, for every operator but `matches`. Rift does the same, except that
it keeps the `<![CDATA[` marker, a processing instruction's target and the DOCTYPE as written, so
the document stays readable (see [Predicates](predicates.md#xpath-predicates)). So:

- A body with a CDATA section still matches a case-insensitive XPath predicate in Rift. In Mountebank it never matches: the lowercased `<![cdata[` is no document to xmldom.
- A body with a DOCTYPE still matches a case-insensitive XPath predicate in Rift. In Mountebank it never matches: the lowercased `<!doctype` is no document to xmldom.
- Rift lowercases the prefixes of `ns` as well as its URIs; Mountebank lowercases only the URIs, so
  in Mountebank an upper-case prefix (`"ns": {"SOAP": …}` with `//SOAP:Body`) is left unbound once
  the selector is lowercased, and the request fails with `500`.
- Rift lowercases ASCII letters only, as every case-insensitive comparison in Rift does; Mountebank
  lowercases any letter.

## Case-insensitive JSONPath

A JSONPath predicate whose keys are not case sensitive matches key names, and (unless it is
`matches` or `caseSensitive: true`) string values, without regard to case, as Mountebank's
lowercasing of the body and the selector does (see [Predicates](predicates.md#jsonpath-predicates)).
Rift lowercases ASCII letters only, as for XPath; Mountebank also folds `É` and every other letter.

## An XPath selector that returns a string

An XPath selector that evaluates to a string, such as `string(//a)`, works in Rift's predicates and `copy`. Mountebank fails the request with a `500` `TypeError` (`result.map is not a function`): its XPath code handles node sets only.

Rift keeps it because it is the way to read an element's whole text: a selector that selects an
element reads, as in Mountebank, only the element's first child node (see
[XPath predicates](predicates.md#xpath-predicates)).

## XPath selectors that do not compile or cannot run

Rift refuses an imposter whose `jsonpath` or `xpath` selector (in a predicate, `copy` or `lookup`)
does not compile, with `400` when it is loaded. Mountebank accepts it, then fails every request that
reaches an XPath one with `500` and never matches a JSONPath one. The same holds for an `inject`
response whose JavaScript does not compile. An imposter that can only fail is reported when it is
loaded.

An XPath selector that compiles but fails when it runs answers `500`, as in Mountebank (see
[XPath predicates](predicates.md#xpath-predicates)). Rift checks the prefixes a selector names
against `ns` before it runs, so it fails on an XML body even when Mountebank's evaluation would never
reach the prefixed step, as in `//absent/x:a` on a document with no `absent` element, or
`//b or x:a` where `//b` is found.

## keyCaseSensitive is honoured as written

Rift applies `keyCaseSensitive` whenever it is set, on every operator; Mountebank applies it only to turn key matching off under `caseSensitive: true`. Three consequences:

- `keyCaseSensitive: true` on its own makes the names in `query`, `headers` and `form` compare
  exactly. In Mountebank it does nothing unless `caseSensitive: true` is also set.
- Under `matches`, Rift honours the setting. Mountebank replaces it with `caseSensitive`, so
  `caseSensitive: true, keyCaseSensitive: false` compares keys exactly there and relaxes them in Rift.
- With a `jsonpath` selector that has an upper-case letter, `keyCaseSensitive: true` still matches in
  Rift; in Mountebank it never matches (the selector keeps its case while the body's keys are
  lowercased). The mirror image holds too: with `keyCaseSensitive: true` and a lower-case selector
  (`$..title`), Mountebank still matches a `Title` key, because it lowercases the body; Rift does not.

Header names are compared against the Title-Cased names the listener stores (`X-Tenant`), because
Rift cannot see the spelling the client sent (see [Request header names](#request-header-names)).
The setting is described in [Predicates](predicates.md#keycasesensitive).

## `/imposters/:port/requests` means recorded requests

`DELETE /imposters/:port/requests` clears the imposter's recorded requests, like `DELETE /imposters/:port/savedRequests`, and `GET /imposters/:port/requests` reads them. In Mountebank the `DELETE` is a deprecated alias of `DELETE /imposters/:port/savedProxyResponses`
and leaves recorded requests alone, and there is no `GET`. To clear saved proxy responses on Rift, use
`savedProxyResponses`.

## Proxy upstream certificates are verified

Mountebank never verifies the certificate of a `proxy` upstream: `httpProxy.js:105` hard-codes `rejectUnauthorized: false`, and no stub field changes it (`cert`, `key`, `passphrase`, `ciphers` and `secureProtocol` are client-side options for mutual auth). Rift verifies the upstream certificate against the OS trust store plus any `--upstream-ca-file`, so a `proxy` to an https upstream with a self-signed or private-CA certificate answers `502` with `x-rift-proxy-error: true` where Mountebank answers from the upstream. This is deliberate: a recording proxy that skips verification records MITM'd traffic as truth.

To get Mountebank's behaviour, start Rift with `--upstream-tls-skip-verify` (or `RIFT_UPSTREAM_TLS_SKIP_VERIFY=true`). To keep verifying against a private CA, use `--upstream-ca-file <pem>`. `--upstream-ca-file` cannot help when the upstream is a Mountebank https imposter on its default certificate: that certificate is X.509 v1 (`CN=mountebank`, no SAN), which Rift's TLS library refuses even as a trust anchor (`UnsupportedCertVersion` in the server log), so for that upstream `--upstream-tls-skip-verify` is the only route. See [TLS](../features/tls.md#trusting-a-private-ca).

`cert`, `key`, `passphrase`, `ciphers` and `secureProtocol` on a `proxy` response are dropped, and Rift cannot present a client certificate to an upstream.

## A proxy without generators records into a store, not a stub

A proxy without `predicateGenerators` (and without `addWaitBehavior`/`addDecorateBehavior`) records into an internal store keyed per request, not into a stub, so `GET /imposters/:port` and `?replayable=true` do not show its recordings. Mountebank records an empty-predicate stub there, which answers every later request with the
first response, whatever its path or method. Rift replays per request instead (method, path, query
string and body). To record exportable stubs, give the proxy `predicateGenerators`, for example
`{"matches": {"method": true, "path": true, "query": true, "body": true}}`. See
[Proxy](proxy.md).

## Generated predicates

A proxy's `predicateGenerators` record predicates whose order, selector scope, selected-value type
and header-name lookup differ from Mountebank 2.9.1's. So do a whole field filtered by `ignore`
(recorded with `equals`), a field under an explicit `predicateOperator` (its value, not the whole
request), and a listed query or form key the request repeats (the array of its values, where
Mountebank records `{}`). Each difference, and why Rift's recording is the one that replays, is
listed under [Proxy: What Gets Recorded](proxy.md#what-gets-recorded).

Mountebank 2.9.5 refuses every generator with `"predicateOperator": "exists"` when the imposter is
created (an internal error with no message), although its docs describe it; Rift records
`{"exists": …}` as documented.

A generator `inject` is dry-run when its stub is created, and one that throws or returns a
non-array is refused, as in Mountebank ([Proxy: The create-time dry run](proxy.md#the-create-time-dry-run)).
The refusal's `data` can word the error differently, and an `inject` that returns a string is
refused where Mountebank records its characters as predicates.

## Two response shapes Mountebank does not have

A stub response carries exactly one of `is`, `proxy`, `inject` and `fault`, and Rift refuses one with
two or with none, with Mountebank's messages. It also accepts two shapes that carry a whole response
without any of those keys, and that Mountebank refuses with `unrecognized response type`:

- the flat form, `statusCode`/`headers`/`body`/`_mode` at the top of the response with no `is`
  wrapper, served as that `is` response (see
  [Responses: Flat response](responses.md#flat-response-no-is-wrapper));
- a response that is only a `_rift` block, whose `_rift.script` builds the response (see
  [Scripting](../features/scripting.md)).

## `defaultResponse` completes a response by the fields it omits

An imposter's `defaultResponse` is served when no stub matches, and it also supplies the `statusCode`, `headers`, `body` and `_mode` that an `is` or `inject` response omits, as in Mountebank. `headers` is taken whole, never merged key by key; a `proxy` response is never completed. Mountebank decides by falsiness and Rift by whether the field was written, which differs in two places:

`defaultResponse` completes a response by the fields it omits: an explicit `"body": ""` is served empty, and a proxied empty body is served as the upstream sent it; Mountebank replaces both with the `defaultResponse` body.

`defaultResponse` is merged before behaviors run. Mountebank merges after them, so there a `decorate` reading `config.response.statusCode` on a response that omits it sees `undefined`; in Rift the decorate sees the completed response, and a default body is expanded like a stub body. A `statusCode` string the default supplies is the exception: no `copy` or `lookup` substitutes into it, as in Mountebank.

A `proxy` inside `defaultResponse` is kept in the printed and persisted imposter and reported as ignored in `_rift.warnings`: neither engine proxies from it. Use `defaultForward` to forward unmatched requests.

## Printed imposters are normalized

Mountebank prints a stub exactly as it was posted. Rift prints what it parsed, so its output can be
spelled differently from the input while meaning the same thing. Re-posting what Rift prints gives
the same imposter.

- `statusCode` prints as a JSON number in a stub's `is`, as it already did in `defaultResponse`. A status posted as a string prints as a number (`"202"` prints as `202`). A string that is not a number, such as a `copy` token, is kept and printed as written, as Mountebank prints it.
- A field left empty or at its default is not printed. This covers a stub's empty `predicates`,
  whether written or omitted, an empty `defaultResponse.headers`, and a proxy's default `mode`,
  `predicateGenerators`, `addWaitBehavior` and `injectHeaders`. Each reads back to the same value.
  A `headers: {}` written in a stub's `is` is still printed, because it replaces the
  `defaultResponse` headers with none.
- Empty `behaviors` and a `null` response variant such as `proxy: null` are accepted and not printed back. Each means nothing, so it reads back the same. Mountebank echoes both as posted, and its `removeProxies` export then drops every response written with a `proxy` key, `proxy: null` included.
- The imposter list carries `recordRequests` for each imposter, next to Rift's `stubCount` and `enabled`. Mountebank prints `recordRequests` only in an imposter's detail.
- The imposter detail carries `enabled` and, when there are any, `_rift` warnings. Mountebank's detail has neither.
- The `?replayable=true` export keeps the imposter's `host`, so a re-posted export binds where the original did. Mountebank 2.9.1's export leaves `host` out.

## An https imposter's TLS keys

Every https view prints the keys Mountebank prints: `mutualAuth`, `rejectUnauthorized` and `ca`
(`null` when unset, otherwise as posted, so a one-element array stays an array), and `ciphers`
upper-cased when set. That includes the create answer, the detail and `?replayable=true`. Four
things differ:

- The default certificate is not printed. When an https imposter has no `key` and `cert`, Mountebank prints its bundled public certificate and key. Rift's fallback is the operator's `--default-tls-cert`/`--default-tls-key`, a secret that would leak through the admin API and every `rift save`, or a certificate generated for that imposter, which an export would turn into stored config. An export without `key` and `cert` re-imports to the same fallback.
- `ciphers` is stored and printed but not applied. Mountebank hands it to OpenSSL as a cipher list; Rift's TLS listener uses rustls' default suites, and the imposter reports the key in `_rift.warnings`.
- A client certificate validated against `ca` (`rejectUnauthorized: true`) must be X.509 v3. Mountebank (OpenSSL) also validates a v1 client certificate; Rift's chain validation is webpki's, which refuses a v1 end-entity, and Rift does not replace it with its own path validation. A v1 certificate is accepted as the imposter's own `cert`, and under `mutualAuth` alone a client may present one.
- Client-auth settings that cannot take effect are refused where Mountebank accepts them: `ca` or `rejectUnauthorized` without `mutualAuth: true`, `ca` without `rejectUnauthorized`, and the other combinations listed under [TLS](../features/tls.md#mutual-tls-mtls). So a Mountebank export carrying `ca` with `mutualAuth: false` does not import into Rift.

## An inherited `--host` is not stored with the imposter

An imposter created without a `host` binds the server's `--host` in both engines, and neither
prints that inherited host. Mountebank writes it into the imposter's `imposter.json` in its
`--datadir`, so the imposter keeps that host when a server with a different `--host` loads it. Rift
stores only what was posted, so a Rift datadir imposter without a `host` follows the `--host` of the
server that loads it.

## `PUT /imposters` that fails part-way

Rift does not delete the running set before a `PUT /imposters`. A port that fails to bind is
reported in `failed` and leaves the others applied, where Mountebank deletes every imposter first
and is left with none. A successful `PUT` gives every imposter in the body fresh runtime state, as
Mountebank's recreate does; `?reconcile=true` keeps the state of unchanged imposters instead (see
[`PUT /imposters`](../api/index.md#put-imposters)).

## `--local-only`

Both engines refuse a client that is not local on every imposter, and neither narrows imposter binds.
Three differences:

- **Refusal point.** Mountebank completes the TLS handshake, logs and parses the request, then
  destroys the socket. Rift refuses at accept, before TLS. Either way the client gets no response
  and the request is not recorded.
- **Admitted set.** Rift admits `127.0.0.0/8`, `::1` and their IPv4-mapped forms. Mountebank admits
  the loopback interface's exact addresses, which adds macOS's `fe80::1%lo0` and, on Linux, excludes
  `127.0.0.2` and above.
- **Admin API and `/metrics`.** Rift binds them to loopback (`127.0.0.1` and `::1`) **and** refuses a
  non-loopback client at accept, as it does on the imposters: a loopback bind alone still accepts a
  connection from another local interface's address, which the kernel routes over loopback (#1568).
  Mountebank binds every interface and refuses non-local clients there. Rift's front door, which Mountebank has no
  counterpart for, refuses the same clients as the imposters.

An imposter cannot take a port this server already listens on: the admin port, `--metrics-port`,
`--front-door` or `--intercept-port`. Such a port is refused with `400` `already in use` on every
platform, with or without `--local-only` (#1557). Mountebank has only the admin listener and refuses
it the same way.

`--ip-whitelist` remains not enforced (#879).

## Connections stay open

Mountebank adds `Connection: close` to every imposter response that does not set a `Connection`
header (`is`, `inject`, `proxy` and the default response when no stub matches), and Node then closes
the socket. Rift sends no `Connection` header and keeps HTTP/1.1 connections alive, so a client
reusing one connection is served again on it. This is deliberate: closing after every response cost
about 5.5x throughput for keep-alive clients in Rift's own measurement, and every published
benchmark measures the keep-alive behaviour.

To get Mountebank's behaviour, set `"Connection": "close"` in the imposter's `defaultResponse`
headers (it applies to every response that does not set its own) or in a single stub's response
headers. Rift honours it: it sends the header and closes the connection. A response that sets
`"Connection": "keep-alive"` stays open in both.

## HTTP framing

Rift frames a response body with `Content-Length` where Mountebank sends it chunked
(`Transfer-Encoding: chunked`), and the order of the response's header fields can differ from
Mountebank's. Both are framing, not content: a client that reads a response as HTTP defines it sees
the same message. Mountebank's own docs examples print raw responses, so the
[mbTest harness](https://github.com/achird-labs/rift/tree/master/conformance/mbtest) compares them
without the framing headers and with the header fields sorted.

## `Content-Length` beside `Transfer-Encoding`

When a response sets both `Content-Length` and `Transfer-Encoding`, Mountebank sends both headers.
Rift sends only `Transfer-Encoding`, because HTTP forbids a message carrying both (RFC 9112 §6.2)
and hyper refuses to send them together. The client reads the same body. Any other
`Content-Length` a response sets is corrected to the body's real length, as Mountebank corrects it
(see [Responses](responses.md#headers)).

## A JSON body gets `Content-Type: application/json`

When an `is` response's `body` is a JSON object or array and the response sets no `Content-Type`,
Rift adds `Content-Type: application/json` (see [Responses](responses.md)). Mountebank serves the
same body, pretty-printed, with no `Content-Type`, so a client that parses by content type reads
Rift's answer as JSON and Mountebank's as text.

## Command line

- `-h` prints help. In `mb` it is `--host`; spell `--host` out. Mountebank's other short flags
  `-p -d -i -l -m -w -o` and `save -r` mean what they mean in `mb` (#1586).
- `--logfile` (an alias of `--log`) has no default. `mb` writes `mb.log` unless told otherwise;
  Rift writes no log file unless one is named.
- `--apikey` (an alias of `--api-key`) reads the key from `x-api-key`, as `mb` does, and also from
  `Authorization`, which `mb` refuses with `401`. When a request sends both, `Authorization`
  decides alone.
- `--ipWhitelist` (an alias of `--ip-whitelist`) is accepted and not enforced: Rift applies no IP
  filtering (#879, see the [CLI reference](../configuration/cli.md#--ip-whitelist-does-not-filter-anything)).
- `--formatter` is accepted and ignored: Rift does not load Mountebank's JavaScript formatter
  modules, so `rift save` always writes JSON (see the [CLI reference](../configuration/cli.md)).
- An `--rcfile` that exists but cannot be used aborts startup. `mb` prints `Cannot parse rcfile`
  and starts for a file that is not JSON; Rift refuses it (and a wrong-typed key, or a path that is
  a directory), because starting without the file would silently drop settings the operator thinks
  are in force (#1114). A path that does not exist warns and starts in both.
- `rift replay --configfile <file>` starts a server with that file loaded. `mb replay` switches a
  running server's proxies to replay mode; Rift has no command for that (see
  [Migration](../getting-started/migration.md)).

## `/metrics`

Rift serves Prometheus metrics on its own listener (`--metrics-port`, default `9090`), not on the
admin port, and names them `rift_*` (see [Metrics](../features/metrics.md)). Mountebank serves
`/metrics` on the admin port, with `mb_*` names such as `mb_predicate_match_duration_seconds` and
`mb_no_match_total`.

## `--debug`

Mountebank's `--debug` lists every match on each stub of `GET /imposters/:id` (`matches`). Rift's
`--debug` raises the log level and enables script tracing. It does not record matches per stub. To
see which stub answered each request, set `recordRequests: true`: each recorded request carries
`matchOutcome.stubIndex`, `status` and `latencyMs`. To ask which stub *would* answer, send the
request with `X-Rift-Debug: true` (see [Debug mode](../features/debug-mode.md)).

## Elsewhere in these docs

Each of these is described where the feature is:

- Served JSON bodies have their keys sorted — [Responses](responses.md).
- `copy` with a `regex` returns the first capture group — [Behaviors](behaviors.md).
- A `lookup` after a `copy` does not expand a token the client put in the copied field — [Behaviors](behaviors.md).
- A `wait` range `{"min", "max"}` is a Rift extension — [Behaviors](behaviors.md).
- String predicate operators fold ASCII case only; `matches` folds Unicode — [Predicates](predicates.md).
- An unreachable proxy upstream answers `502` — [Proxy](proxy.md).
- An unknown proxy `mode` is refused; Mountebank treats it as `proxyOnce` — [Proxy](proxy.md#proxy-modes).
- `protocol` is optional and defaults to `http` — [Imposters](imposters.md).
- A proxy-recorded stub carries `recordedFrom` (Rift only, per stub) — [Imposters](imposters.md). Each recorded response also carries `_proxyResponseTime`, the per-response mark both engines share — [Proxy](proxy.md).
- Rift-only config keys print with `?replayable=true`, and the error envelope carries `type` — [API](../api/index.md).
