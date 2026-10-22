# rift-ejs

The EJS template subset that [Rift](https://github.com/achird-labs/rift)'s config loader evaluates,
shared with `rift-lint` so the server and the linter read a templated config file the same way.

Supported tags, the forms Mountebank's `--configfile` renderer (EJS 2.7.4) is used with:

| Tag | Result |
|:----|:-------|
| `<% include file %>`, `<%- include('file') %>` | Inlines the file, itself rendered, relative to the file the tag is in (`.ejs` appended without an extension). |
| `<%- stringify('file') %>`, `<%- stringify(filename, 'file') %>` | Inlines the file, rendered, trimmed and escaped for a JSON string, relative to the root config file at any depth. |
| `<%- stringify(filename, 'file', {key: 'v'}) %>` | The same, with `<%- data.key %>` / `<%= data.key %>` reading the values in that file. |
| `<%- inject(...) %>` | Mountebank's older name for `stringify`. |
| `<%= process.env.VAR \|\| 'default' %>` | The variable's value (or the default, which is optional), HTML-escaped; empty if unset (reported, as Mountebank does). |
| `<%- process.env.VAR \|\| 'default' %>` | The same, raw. |

`-%>` drops the line break after the tag. Includes and stringifies nest up to 16 files deep, and a
file that renders itself is an error naming the chain.

Any other tag fails the render with an error naming the tag and where it is, instead of being
stripped — a config that loads differently from the file is worse than one that does not load.
Text a substitution inserts is never scanned again as template.

The crate has no Rift dependencies. Rendering is documented on `rift_ejs::render`; the user-facing
rules are in [Configuration](https://achird-labs.github.io/rift/configuration/).

## License

Apache-2.0
