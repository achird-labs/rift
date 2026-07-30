// Modeled on a decorate module from a real-world Mountebank migration corpus:
//   injection_files/injection.cjs
//
// Migration probe: a `decorate` module loaded via require(). Exercises
//   - config.request.body  + config.response.body (both STRING, JSON.parse'd)
//   - copies the requested variant attribute onto every response buyRateVariant
// See the conformance corpus manifest (lender service).
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
