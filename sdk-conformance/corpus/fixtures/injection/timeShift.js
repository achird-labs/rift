// Verbatim from cof-primary/RENUW-renuw-mimeo-solo/services/
//   COAFPerformanceDeclineLookupLlds/v1/url/injection/timeShift.js
//
// Migration probe: a `decorate` module loaded via require(). Exercises
//   - config.stub.scenarioName  (scenario name available to the decorate)
//   - config.response.body as a PARSED OBJECT (datasetRecords array walk)
// The scenario name (…_less_than_48_hours) drives the hour offset it stamps
// onto lldsRecordCreatedTime. See scripts/conformance-migration.sh (RENUW · COAF).
module.exports = function (config) {
    // scenario name usually available as config.stub.scenarioName in decorate
    const scenarioName =
        (config && config.stub && config.stub.scenarioName ? config.stub.scenarioName : "").toLowerCase();

    // Map scenario name patterns -> hour offsets
    let hours = null;

    if (scenarioName.includes("less_than_24_hours")) {
        hours = 12;
    } else if (scenarioName.includes("less_than_48_hours")) {
        hours = 36;
    } else if (scenarioName.includes("equals_to_48_hours")) {
        hours = 48;
    } else if (scenarioName.includes("more_than_48_hours")) {
        hours = 50;
    }

    // If scenario doesn't match any time pattern, do nothing
    if (hours === null) {
        return;
    }

    const dt = new Date(Date.now() - hours * 60 * 60 * 1000).toISOString();

    // Update only when field exists
    if (
        config.response &&
        config.response.body &&
        config.response.body.datasetRecords &&
        Array.isArray(config.response.body.datasetRecords)
    ) {
        config.response.body.datasetRecords.forEach((r) => {
            if (r && Object.prototype.hasOwnProperty.call(r, "lldsRecordCreatedTime")) {
                r.lldsRecordCreatedTime = dt;
            }
        });
    }
};
