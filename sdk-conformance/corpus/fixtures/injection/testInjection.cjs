// Stand-in for cof-primary/RENUW-renuw-mimeo-solo/.../injection_files/testInjection.cjs
// (the hand-written scaffold template references it but ships no body). Minimal,
// faithful to the scaffold's intent: stamp a header proving the require() ran.
//
// Migration probe: decorate via `(config) => { require(...)(config); }`.
// See scripts/conformance-migration.sh (RENUW · imposter scaffold).
module.exports = function (config) {
  config.response.headers = config.response.headers || {};
  config.response.headers['X-Injected-By'] = 'testInjection.cjs';
};
