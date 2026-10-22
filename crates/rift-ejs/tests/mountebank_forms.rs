//! Issue #1604: the tag forms Mountebank's `--configfile` renderer (EJS 2.7.4, through
//! `@mbtest/mountebank-formatters`) evaluates, beyond the original subset, and the refusals around
//! them. Each expected value here was checked against that renderer.

use rift_ejs::{EjsError, FileAccess, Rendered, render};
use std::path::{Path, PathBuf};

/// A config directory holding `files`, with the root document written as `imposters.ejs`.
struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write fixture");
        }
        Self { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("imposters.ejs")
    }

    fn render(&self, doc: &str) -> Result<Rendered, EjsError> {
        render(doc, &self.root(), FileAccess::Allowed)
    }

    fn text(&self, doc: &str) -> String {
        match self.render(doc) {
            Ok(rendered) => rendered.text,
            Err(e) => panic!("{doc:?} should render: {e}"),
        }
    }

    fn error(&self, doc: &str) -> String {
        match self.render(doc) {
            Ok(rendered) => panic!("{doc:?} should be refused, rendered {:?}", rendered.text),
            Err(e) => e.to_string(),
        }
    }
}

// ===== include =====

#[test]
fn include_call_forms_inline_the_file() {
    let tree = Tree::new(&[("a.json", "{\"a\":1}")]);
    for doc in [
        "[<%- include('a.json') %>]",
        "[<%- include(\"a.json\") %>]",
        "[<%-include( 'a.json' )%>]",
        "[<%- include('a.json') -%>]",
        "[<% include a.json %>]",
        "[<% include 'a.json' %>]",
        "[<%- include a.json %>]",
        "[<% include a.json -%>]",
    ] {
        assert_eq!(tree.text(doc), "[{\"a\":1}]", "{doc}");
    }
}

#[test]
fn a_dash_close_drops_one_following_line_break() {
    let tree = Tree::new(&[("a.json", "A")]);
    assert_eq!(
        tree.text("<%- include('a.json') -%>\n\nB"),
        "A\nB",
        "only one newline is dropped"
    );
    assert_eq!(tree.text("<%- include('a.json') -%>\r\nB"), "AB");
    assert_eq!(tree.text("<%- include('a.json') -%>\rB"), "AB");
    assert_eq!(tree.text("<%- include('a.json') -%> \nB"), "A \nB");
    assert_eq!(
        tree.text("<%- include('a.json') %>\nB"),
        "A\nB",
        "a plain close keeps it"
    );
}

#[test]
fn an_include_without_an_extension_reads_the_ejs_file() {
    let tree = Tree::new(&[("part.ejs", "P")]);
    assert_eq!(tree.text("<%- include('part') %>"), "P");
}

#[test]
fn includes_nest_and_resolve_against_the_including_file() {
    let tree = Tree::new(&[
        ("a.json", "ROOT-A"),
        ("sub/a.json", "SUB-A"),
        ("sub/inc.ejs", "X<%- include('a.json') %>Y"),
        ("sub/deeper/leaf.json", "LEAF"),
        ("sub/mid.ejs", "<% include deeper/leaf.json %>"),
    ]);
    assert_eq!(tree.text("<%- include('sub/inc.ejs') %>"), "XSUB-AY");
    assert_eq!(tree.text("<%- include('sub/mid.ejs') %>"), "LEAF");
}

#[test]
fn an_include_with_an_absolute_path_reads_that_path() {
    let tree = Tree::new(&[("abs.json", "ABS")]);
    let abs = tree.dir.path().join("abs.json");
    assert_eq!(
        tree.text(&format!("<%- include('{}') %>", abs.display())),
        "ABS"
    );
}

#[test]
fn an_escaping_include_is_refused_with_a_hint() {
    let tree = Tree::new(&[("a.json", "{}")]);
    let err = tree.error("<%= include('a.json') %>");
    assert!(
        err.starts_with("unsupported EJS tag `<%= include('a.json') %>` at ")
            && err.contains("write `<%-`"),
        "{err}"
    );
}

#[test]
fn an_include_with_a_data_argument_is_refused() {
    let tree = Tree::new(&[("a.json", "{}")]);
    let err = tree.error("<%- include('a.json', {a: 1}) %>");
    assert!(
        err.starts_with("unsupported EJS tag `<%- include('a.json', {a: 1}) %>` at "),
        "{err}"
    );
}

// ===== stringify / inject =====

#[test]
fn stringify_and_inject_forms_inline_the_escaped_file() {
    let tree = Tree::new(&[("f.txt", "  a \"b\"\n")]);
    for doc in [
        "\"<%- stringify('f.txt') %>\"",
        "\"<%- stringify(filename, 'f.txt') %>\"",
        "\"<%- stringify( filename , \"f.txt\" ) %>\"",
        "\"<%- inject('f.txt') %>\"",
        "\"<%- inject(filename, 'f.txt') %>\"",
        "\"<%- stringify(filename, 'f.txt', {}) %>\"",
        "\"<%- stringify(filename, 'f.txt') -%>\"",
    ] {
        assert_eq!(tree.text(doc), r#""a \"b\"""#, "{doc}");
    }
}

#[test]
fn stringify_resolves_against_the_root_file_at_any_depth() {
    let tree = Tree::new(&[
        (
            "services/data.ejs",
            "\"<%- stringify(filename, 'services/body.txt') %>\"",
        ),
        ("services/body.txt", "ROOT-RELATIVE"),
        (
            "services/services/body.txt",
            "WRONG: relative to the including file",
        ),
    ]);
    assert_eq!(
        tree.text("<%- include('services/data.ejs') %>"),
        "\"ROOT-RELATIVE\""
    );
}

#[test]
fn an_absolute_stringify_path_reads_that_path_from_any_depth() {
    // Rift's documented behaviour (the TLS and Kubernetes guides stringify `/certs/...`), kept:
    // Mountebank's `path.join` would look under the root's directory instead.
    let tree = Tree::new(&[
        ("certs/key.pem", "KEY"),
        ("sub/part.json", "\"<%- stringify(filename, 'ABS') %>\""),
    ]);
    let abs = tree.dir.path().join("certs/key.pem");
    let part = std::fs::read_to_string(tree.dir.path().join("sub/part.json"))
        .expect("read part")
        .replace("ABS", &abs.display().to_string());
    std::fs::write(tree.dir.path().join("sub/part.json"), part).expect("write part");
    assert_eq!(
        tree.text(&format!("\"<%- stringify('{}') %>\"", abs.display())),
        "\"KEY\""
    );
    assert_eq!(tree.text("<%- include('sub/part.json') %>"), "\"KEY\"");
}

#[test]
fn stringifies_nest_and_an_include_in_a_stringified_file_resolves_against_the_root() {
    let tree = Tree::new(&[
        ("body.txt", "{\"ok\": true}"),
        ("outer.js", "x = '<%- stringify(filename, 'body.txt') %>';"),
        ("a.json", "ROOT-A"),
        ("sub/a.json", "SUB-A"),
        ("sub/inc.ejs", "<%- include('a.json') %>"),
    ]);
    assert_eq!(
        tree.text("\"<%- stringify(filename, 'outer.js') %>\""),
        r#""x = '{\\\"ok\\\": true}';""#
    );
    // EJS renders a stringified file with the root config as its `filename`, so an include in it
    // looks beside the root, while one in an included file looks beside that file.
    assert_eq!(
        tree.text("\"<%- stringify(filename, 'sub/inc.ejs') %>\""),
        "\"ROOT-A\""
    );
    assert_eq!(tree.text("<%- include('sub/inc.ejs') %>"), "SUB-A");
}

#[test]
fn stringify_data_is_rendered_raw_or_escaped() {
    let tree = Tree::new(&[
        ("raw.ejs", "<%- data.v %>"),
        ("esc.ejs", "<%= data.v %>"),
        ("two.ejs", "<%- data.a %>-<%- data['b'] %>"),
    ]);
    for (args, rendered) in [
        ("{v: 'x'}", "x"),
        ("{ v : \"x\" , }", "x"),
        ("{'v': 1111}", "1111"),
        ("{\"v\": 1.50}", "1.5"),
        ("{v: -0.10}", "-0.1"),
        ("{v: true}", "true"),
        ("{v: false}", "false"),
        ("{other: 'x'}", ""),
        ("{v: 'a\\'b'}", "a'b"),
        ("{v: 'one', v: 'two'}", "two"),
    ] {
        assert_eq!(
            tree.text(&format!("<%- stringify(filename, 'raw.ejs', {args}) %>")),
            rendered,
            "{args}"
        );
    }
    assert_eq!(
        tree.text("<%- stringify(filename, 'esc.ejs', {v: '<&>\"\\''}) %>"),
        "&lt;&amp;&gt;&#34;&#39;"
    );
    // `data['b']` is not a supported expression.
    let err = tree.error("<%- stringify(filename, 'two.ejs', {a: 1, b: 2}) %>");
    assert!(
        err.starts_with("unsupported EJS tag `<%- data['b'] %>` at two.ejs:1, stringified at "),
        "{err}"
    );
}

#[test]
fn data_reaches_a_file_included_from_a_stringified_file() {
    let tree = Tree::new(&[
        ("outer.ejs", "<%- include('inner.ejs') %>"),
        ("inner.ejs", "<%- data.v %>"),
    ]);
    assert_eq!(
        tree.text("<%- stringify(filename, 'outer.ejs', {v: 'passed'}) %>"),
        "passed"
    );
}

#[test]
fn data_outside_a_stringify_with_data_is_refused() {
    let tree = Tree::new(&[("d.ejs", "<%- data.v %>")]);
    for doc in ["<%- data.v %>", "<%- stringify(filename, 'd.ejs') %>"] {
        let err = tree.error(doc);
        assert!(
            err.starts_with("unsupported EJS tag `<%- data.v %>` at ")
                && err
                    .contains("`data` is only set inside a file stringified with a data argument"),
            "{doc}: {err}"
        );
    }
}

#[test]
fn rich_data_arguments_are_refused() {
    let tree = Tree::new(&[("d.ejs", "<%- data.v %>")]);
    for args in [
        "{v: {nested: 1}}",
        "{v: [1]}",
        "{v: 1 + 1}",
        "{v: someVar}",
        "{v: 1e3}",
        "{v: 'a' 'b'}",
        "{v: 'unterminated}",
        "{v: '\\x41'}",
        "data",
    ] {
        let doc = format!("<%- stringify(filename, 'd.ejs', {args}) %>");
        let err = tree.error(&doc);
        assert!(
            err.starts_with(&format!("unsupported EJS tag `{doc}` at ")),
            "{args}: {err}"
        );
    }
}

#[test]
fn the_two_argument_object_form_is_refused_naming_the_working_form() {
    // Mountebank's own docs show `stringify('file.ejs', { value: 'first' })`, which crashes
    // Mountebank 2.9.5: with two arguments the object lands in the file-name parameter.
    let tree = Tree::new(&[("f.ejs", "x")]);
    let err = tree.error("<%- stringify('f.ejs', { value: 'first' }) %>");
    assert!(
        err.starts_with("unsupported EJS tag `<%- stringify('f.ejs', { value: 'first' }) %>` at ")
            && err.contains("Mountebank's renderer fails on this form too")
            && err.contains("stringify(filename, 'file', {...})"),
        "{err}"
    );
}

#[test]
fn an_escaping_stringify_is_refused_with_a_hint() {
    let tree = Tree::new(&[("f.txt", "x")]);
    let err = tree.error("<%= stringify('f.txt') %>");
    assert!(
        err.starts_with("unsupported EJS tag `<%= stringify('f.txt') %>` at ")
            && err.contains("write `<%-`"),
        "{err}"
    );
}

// ===== process.env =====

#[test]
fn an_escaping_env_tag_html_escapes_like_ejs_and_a_raw_one_does_not() {
    // Safety: a name no other test reads, set once and never removed.
    unsafe { std::env::set_var("RIFT_EJS_TEST_1604_SPECIAL", "a&b<c>\"d'e") };
    unsafe { std::env::remove_var("RIFT_EJS_TEST_1604_UNSET") };
    let tree = Tree::new(&[]);
    assert_eq!(
        tree.text("<%= process.env.RIFT_EJS_TEST_1604_SPECIAL %>"),
        "a&amp;b&lt;c&gt;&#34;d&#39;e"
    );
    assert_eq!(
        tree.text("<%- process.env.RIFT_EJS_TEST_1604_SPECIAL %>"),
        "a&b<c>\"d'e"
    );
    assert_eq!(
        tree.text("<%= process.env.RIFT_EJS_TEST_1604_UNSET || '<x>' %>"),
        "&lt;x&gt;"
    );
    assert_eq!(
        tree.text("<%- process.env.RIFT_EJS_TEST_1604_UNSET || '<x>' %>"),
        "<x>"
    );
    let rendered = tree
        .render("<%- process.env.RIFT_EJS_TEST_1604_UNSET %>")
        .expect("renders");
    assert_eq!(rendered.text, "");
    assert_eq!(rendered.unset_env.len(), 1, "a raw tag reports unset too");
}

// ===== recursion =====

#[test]
fn a_file_that_includes_itself_is_refused_naming_the_chain() {
    let tree = Tree::new(&[
        ("loop.ejs", "<%- include('loop.ejs') %>"),
        ("a.ejs", "<%- include('b.ejs') %>"),
        ("b.ejs", "<%- include('a.ejs') %>"),
        ("s.ejs", "<%- stringify(filename, 's.ejs') %>"),
        ("back.ejs", "<%- include('imposters.ejs') %>"),
    ]);
    for (doc, chain) in [
        (
            "<%- include('loop.ejs') %>",
            "imposters.ejs → loop.ejs → loop.ejs",
        ),
        (
            "<%- include('a.ejs') %>",
            "imposters.ejs → a.ejs → b.ejs → a.ejs",
        ),
        (
            "<%- stringify(filename, 's.ejs') %>",
            "imposters.ejs → s.ejs → s.ejs",
        ),
        (
            "<%- include('back.ejs') %>",
            "imposters.ejs → back.ejs → imposters.ejs",
        ),
        // The chain shows each path as its tag wrote it; `./loop.ejs` and `loop.ejs` are one file.
        (
            "<%- include('./loop.ejs') %>",
            "imposters.ejs → ./loop.ejs → loop.ejs",
        ),
    ] {
        let err = tree.error(doc);
        assert!(
            err.contains("would never finish") && err.contains(chain),
            "{doc}: {err}"
        );
    }
}

#[test]
fn nesting_deeper_than_sixteen_files_is_refused() {
    let mut files: Vec<(String, String)> = (0..20)
        .map(|i| {
            (
                format!("n{i}.ejs"),
                format!("<%- include('n{}.ejs') %>", i + 1),
            )
        })
        .collect();
    files.push(("n20.ejs".to_string(), "END".to_string()));
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_str()))
        .collect();
    let tree = Tree::new(&borrowed);
    let err = tree.error("<%- include('n0.ejs') %>");
    assert!(
        err.contains("nest deeper than 16 files") && err.contains("n15.ejs → n16.ejs"),
        "{err}"
    );
    // Sixteen levels are fine.
    assert_eq!(tree.text("<%- include('n5.ejs') %>"), "END");
}

#[test]
fn the_same_file_twice_side_by_side_is_not_a_cycle() {
    let tree = Tree::new(&[("d.ejs", "<%- data.v %>")]);
    assert_eq!(
        tree.text(
            "<%- stringify(filename, 'd.ejs', {v: 1}) %>,<%- stringify(filename, 'd.ejs', {v: 2}) %>"
        ),
        "1,2"
    );
}

// ===== error places =====

#[test]
fn an_error_in_a_nested_include_names_the_whole_chain() {
    let tree = Tree::new(&[
        ("a.ejs", "\n<%- include('b.json') %>"),
        ("b.json", "{\n\n<%= nope %>}"),
    ]);
    let err = tree.error("{\n\n<%- include('a.ejs') %>}");
    let root = tree.root();
    assert!(
        err.starts_with(&format!(
            "unsupported EJS tag `<%= nope %>` at b.json:3, included at a.ejs:2, included at {}:3",
            root.display()
        )),
        "{err}"
    );
}

#[test]
fn a_missing_nested_file_names_the_resolved_path() {
    let tree = Tree::new(&[("sub/a.ejs", "<%- include('gone.json') %>")]);
    let err = tree.error("<%- include('sub/a.ejs') %>");
    let expected = tree.dir.path().join("sub").join("gone.json");
    assert!(
        err.starts_with(&format!(
            "EJS include file 'gone.json' not found ({}): ",
            expected.display()
        )),
        "{err}"
    );
}

#[test]
fn stringified_spans_cover_stringifies_in_included_files() {
    let tree = Tree::new(&[
        (
            "part.json",
            "{\"b\": \"<%- stringify(filename, 'f.txt') %>\"}",
        ),
        ("f.txt", "V"),
    ]);
    let rendered = tree
        .render("[\n<%- include('part.json') -%>\n]")
        .expect("renders");
    assert_eq!(rendered.text, "[\n{\"b\": \"V\"}]");
    assert_eq!(rendered.stringified.len(), 1);
    let span = &rendered.stringified[0];
    assert_eq!(&rendered.text[span.range.clone()], "V");
    assert!(span.quoted);
    assert!(
        span.place.starts_with("at part.json:1, included at ") && span.place.ends_with(":2"),
        "{}",
        span.place
    );
}

// ===== fetched documents =====

#[test]
fn a_fetched_document_refuses_every_file_reading_spelling() {
    let fetched = Path::new("https://h/imposters.json");
    for (doc, label, file) in [
        (
            "<% include 'secret.json' %>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<% include secret.json %>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<% include secret.json -%>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<%- include secret.json %>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<%- include('secret.json') %>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<%- include(\"secret.json\") -%>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<%= include('secret.json') %>",
            "<% include ... %>",
            "secret.json",
        ),
        (
            "<%- stringify('secret.js') %>",
            "<%- stringify(...) %>",
            "secret.js",
        ),
        (
            "<%- stringify(filename, 'secret.js') %>",
            "<%- stringify(...) %>",
            "secret.js",
        ),
        (
            "<%- stringify(filename, 'secret.js', {a: 1}) %>",
            "<%- stringify(...) %>",
            "secret.js",
        ),
        (
            "<%- inject('secret.js') %>",
            "<%- inject(...) %>",
            "secret.js",
        ),
        (
            "<%- inject(filename, 'secret.js') -%>",
            "<%- inject(...) %>",
            "secret.js",
        ),
    ] {
        let err = render(
            &format!("{{\"a\": \"<%= process.env.HOME %>\", \"b\": \"{doc}\"}}"),
            fetched,
            FileAccess::Denied,
        )
        .expect_err(doc);
        assert!(matches!(err, EjsError::LocalFileRefused(_)), "{doc}: {err}");
        let message = err.to_string();
        assert!(
            message.starts_with(&format!("`{label}` reads a local file"))
                && message.contains(&format!("it names '{file}'")),
            "{doc}: {message}"
        );
    }
}
