// Regenerates each fixture's expected.json with Mountebank's own --configfile renderer
// (@mbtest/mountebank-formatters, which renders with EJS 2.7.4). Run from this directory:
//   node generate.js <path to a mountebank 2.9.5 install's node_modules>
// e.g. node generate.js ~/bench-mb/node_modules
'use strict';
const path = require('path');
const fs = require('fs');
const formatter = require(path.join(process.argv[2], '@mbtest/mountebank-formatters/src/default.js'));
for (const dir of ['imposters', 'templates', 'nestedStringify', 'dataStringify', 'legacyInclude']) {
    const configfile = path.join(__dirname, dir, 'imposters.ejs');
    const { imposters } = formatter.load({ configfile });
    fs.writeFileSync(path.join(__dirname, dir, 'expected.json'), JSON.stringify({ imposters }, null, 2) + '\n');
}
