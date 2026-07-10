// Verbatim from cof-primary/RENUW-renuw-mimeo-solo/services/Renuw_API_Executor/
//   v1/postAPIRequest/injection/appendExecutionId.cjs
//
// Migration probe: a Mountebank `decorate` module loaded via require(). Exercises
//   - config.path         (shorthand request path on the decorate config)
//   - config.request.headers
//   - config.response.body as a STRING mutated via JSON.parse/JSON.stringify
// See scripts/conformance-migration.sh (RENUW · Renuw_API_Executor).
module.exports = function (config) {
    // Extract execution ID from request URL
    var executionId = null;

    if (config.path) {
        var urlParts = config.path.split('/');
        var lastSegment = urlParts[urlParts.length - 1];

        // Check if last segment is a valid UUID (36 characters)
        if (lastSegment && lastSegment.length === 36) {
            executionId = lastSegment;
        }
    }

    // Only proceed if we have an execution ID
    if (!executionId) {
        return;
    }

    // Parse existing response body
    var body = {};
    if (config.response.body) {
        try {
            body = JSON.parse(config.response.body);
        } catch (e) {
            body = {};
        }
    }

    // Add executionId to response body
    body.executionId = executionId;

    // Update response
    config.response.statusCode = 200;
    config.response.body = JSON.stringify(body);

    // Ensure headers object exists and set content type
    config.response.headers = config.response.headers || {};
    config.response.headers['Content-Type'] = 'application/json';
};
