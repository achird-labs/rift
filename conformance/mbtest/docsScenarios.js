'use strict';
// One mocha test per docs scenario (#1547). mbTest's own web/docsIntegrityTest.js gives one result
// per page and stops at the page's first failing step, so a single framing difference would hide a
// whole page. This adapter uses mbTest's own docs.getScenarios() and runs every scenario on its own.
//
// Run with `mocha --delay` from the mbTest directory, with:
//   MBTEST_DIR  the pinned checkout's mbTest directory
//   MB_PORT     the static server holding the docs snapshot (docs.js reads the pages from it)
//   ENGINE_PORT the engine under test (the port the snapshot's examples were rendered with)
//
// The only change to what docs.js compares is the HTTP normaliser below, applied to both engines
// alike. It never touches a status line or a body.
const path = require('path'),
    assert = require('assert'),
    mbtest = process.env.MBTEST_DIR,
    enginePort = parseInt(process.env.ENGINE_PORT || '2525'),
    docs = require(path.join(mbtest, 'web/docsTester/docs')),
    api = require(path.join(mbtest, 'api')).create(enginePort);

// The fourteen pages mbTest's web/docsIntegrityTest.js validates, in its order.
const pages = [
    '/docs/api/mocks',
    '/docs/api/proxies',
    '/docs/api/injection',
    '/docs/api/xpath',
    '/docs/api/json',
    '/docs/protocols/https',
    '/docs/protocols/http',
    '/docs/api/jsonpath',
    '/docs/api/overview',
    '/docs/gettingStarted',
    '/docs/api/predicates',
    '/docs/api/behaviors',
    '/docs/api/stubs',
    '/docs/protocols/tcp'
];

// Compare a printed HTTP response as a message, not as a byte layout. Each rule cites the
// documented difference that makes it safe, the way conformance/differential/allowlist.json does.
const stepFailure = 'failed; below is the actual result',
    dropped = [
        // docs/mountebank/differences.md#marker-headers-on-served-responses
        /^x-rift-[^:]*:/i,
        // docs/mountebank/differences.md#http-framing: Rift frames a body with Content-Length where
        // Mountebank chunks it.
        /^(content-length|transfer-encoding):/i,
        // docs/mountebank/differences.md#connections-stay-open: Mountebank's default
        // `Connection: close` is not sent by Rift; the API phase reports it on its own
        // (expected.json), and dropping it here keeps it from hiding every docs scenario
        // behind one finding.
        /^connection:/i
    ],
    normalise = text => {
        if (typeof text !== 'string' || !/^HTTP\/1\.1 /.test(text)) {
            return text;
        }
        const split = text.indexOf('\n\n'),
            head = split < 0 ? text : text.slice(0, split),
            body = split < 0 ? '' : text.slice(split),
            [status, ...headers] = head.split('\n'),
            kept = headers
                .filter(line => !dropped.some(rule => rule.test(line)))
                // Header-name case: Rift writes names in lower case on the wire, a bug the API phase
                // reports on its own (#1597). Only an `exec` step (curl -i)
                // sees it; an `http` step prints node's names re-cased the same way for both.
                .map(line => line.replace(/^([^:]+):/, (match, name) => `${name.toLowerCase()}:`))
                // docs/mountebank/differences.md#http-framing: header order carries no meaning.
                .sort();
        return [status, ...kept].join('\n') + body;
    },
    strictEqual = assert.strictEqual;

// docs.js compares a whole step with assert.strictEqual(actual, expected, failureMessage); only
// those calls (recognised by docsTestScenario.js's message) are normalised.
assert.strictEqual = (actual, expected, message) =>
    (typeof message === 'string' && message.includes(stepFailure))
        ? strictEqual(normalise(actual), normalise(expected), message)
        : strictEqual(actual, expected, message);

(async () => {
    const scenarios = {};
    for (const page of pages) {
        scenarios[page] = await docs.getScenarios(page);
    }

    // Each page is a suite, each scenario a test: the id check.py builds is docs:<page>#<scenario>.
    pages.forEach(page => {
        describe(page, function () {
            this.timeout(parseInt(process.env.MB_SLOW_TEST_TIMEOUT || 8000));

            afterEach(async function () {
                await api.del('/imposters');
            });

            Object.keys(scenarios[page]).forEach(name => {
                it(name, async function () {
                    await scenarios[page][name].assertValid();
                });
            });
        });
    });
    run();
})().catch(error => {
    console.error(`docsScenarios: could not read the docs snapshot: ${error.stack || error}`);
    process.exit(1); // eslint-disable-line no-process-exit
});
