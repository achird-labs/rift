// Stand-in for an injection file from a real-world Mountebank migration corpus
// (the hand-written scaffold template references it but ships no body). Minimal,
// faithful to the scaffold's intent: stamp a header proving the require() ran.
//
// Migration probe: decorate via `(config) => { require(...)(config); }`.
// See the conformance corpus manifest (imposter scaffold).
module.exports = function (config) {
  config.response.headers = config.response.headers || {};
  config.response.headers['X-Injected-By'] = 'testInjection.cjs';
};
