// Verbatim from cof-primary/auto-digital-digi-rtl-mimeo-solo/services/Kaizen/
//   injection_files/injection.cjs
//
// Migration probe: a `decorate` module loaded via require(). Exercises
//   - config.request.body  + config.response.body (both STRING, JSON.parse'd)
//   - copies the requested variant attribute onto every response buyRateVariant
// See scripts/conformance-migration.sh (auto-digital · Kaizen).
module.exports = function (config) {
  const req = config.request;
  const res = config.response;
  const reqBody = JSON.parse(req.body);
  const requestedVariant = reqBody.variant.variantAttribute;
  const respBody = JSON.parse(res.body);
  if (!respBody.buyRateVariants) {
    return;
  }
  for (const variant of respBody.buyRateVariants) {
    if (variant.variantAttribute) {
      variant.variantAttribute = requestedVariant;
    }
  }
  res.body = JSON.stringify(respBody);
};
