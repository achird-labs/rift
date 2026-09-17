# rift-ejs

The EJS template subset that [Rift](https://github.com/achird-labs/rift)'s config loader evaluates,
shared with `rift-lint` so the server and the linter read a templated config file the same way.

Supported tags, as emitted by Mountebank and compatible tooling:

| Tag | Result |
|:----|:-------|
| `<% include 'path' %>` | Inlines the referenced file, relative to the config file. |
| `<%- stringify('path') %>` | Inlines a file's rendered contents, escaped for a JSON string. |
| `<%= process.env.VAR %>` | The variable's value; empty if unset (reported, as Mountebank does). |
| `<%= process.env.VAR \|\| 'default' %>` | The variable's value, or the literal default. |

Any other tag fails the render with an error naming the tag and where it is, instead of being
stripped — a config that loads differently from the file is worse than one that does not load.
Text a substitution inserts is never scanned again as template.

The crate has no Rift dependencies. Rendering is documented on `rift_ejs::render`; the user-facing
rules are in [Configuration](https://achird-labs.github.io/rift/configuration/).

## License

Apache-2.0
