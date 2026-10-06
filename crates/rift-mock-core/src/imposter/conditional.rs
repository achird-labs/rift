//! Declarative conditional GET (issue #1280): `_rift.conditional` on an `is` response.
//!
//! RFC 7232, for a `GET` or `HEAD` answered with a 2xx: the response carries a strong `ETag` over
//! the bytes served and a `Last-Modified`. A request whose `If-None-Match` names that tag (or is
//! `*`), or that has no `If-None-Match` and an `If-Modified-Since` no earlier than `Last-Modified`,
//! gets a bodyless `304` instead. Any other request is served unchanged, without validators.
//!
//! The served bytes are known only at the end of the slow path (templating, behaviors and date
//! tokens all change them), so the ETag is computed there per request; the prepared fast path
//! computes it once, at construction.

use bytes::Bytes;
use chrono::{DateTime, NaiveDateTime, Utc};
use http_body_util::Full;
use hyper::header::{ETAG, HeaderName, LAST_MODIFIED};
use hyper::{Response, StatusCode};

use super::reconcile::Fnv1a;
use super::types::{ConditionalGet, LastModified};
use crate::util::FastMap;

const IMF_FIXDATE: &str = "%a, %d %b %Y %H:%M:%S GMT";

/// The headers of the configured response a 304 keeps besides the validators: the ones RFC 7232
/// §4.1 requires a 304 to repeat. `Date` is the server's own.
const KEPT_ON_304: [&str; 4] = ["cache-control", "content-location", "expires", "vary"];

/// What a `_rift.conditional` block resolves to before any request arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConditionalSpec {
    etag: bool,
    last_modified: Stamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stamp {
    Load,
    /// Served verbatim. `secs` is `None` only for a date admission refuses, which a config built
    /// in code can still carry: there is nothing to compare an `If-Modified-Since` with, so it
    /// never answers 304.
    Fixed {
        literal: String,
        secs: Option<i64>,
    },
}

impl ConditionalSpec {
    /// `None` when the block is absent or `false`.
    pub(crate) fn from_config(config: Option<&ConditionalGet>) -> Option<Self> {
        let (etag, last_modified) = config?.resolved()?;
        let last_modified = match last_modified {
            LastModified::Load => Stamp::Load,
            LastModified::Fixed(literal) => Stamp::Fixed {
                secs: parse_imf_fixdate(literal),
                literal: literal.clone(),
            },
        };
        Some(Self {
            etag,
            last_modified,
        })
    }

    /// Whether the response carries an `ETag`, which the caller computes over the served bytes.
    pub(crate) fn wants_etag(&self) -> bool {
        self.etag
    }

    /// The validators for one response: `etag` as computed by the caller (`None` when
    /// [`wants_etag`](Self::wants_etag) is false), and the stub's load time for `"load"`.
    pub(crate) fn validators(&self, etag: Option<String>, loaded_at: DateTime<Utc>) -> Validators {
        let (last_modified, last_modified_secs) = match &self.last_modified {
            Stamp::Load => (
                loaded_at.format(IMF_FIXDATE).to_string(),
                Some(loaded_at.timestamp()),
            ),
            Stamp::Fixed { literal, secs } => (literal.clone(), *secs),
        };
        Validators {
            etag,
            last_modified,
            last_modified_secs,
        }
    }
}

/// Whether a request method can be answered conditionally: only the safe reads (RFC 7232 §3.3).
pub(crate) fn method_applies(method: &str) -> bool {
    method == "GET" || method == "HEAD"
}

/// Whether a response to `method` with `status` is conditional at all. A non-2xx is never
/// replaced by a 304.
pub(crate) fn applies(method: &str, status: u16) -> bool {
    method_applies(method) && (200..300).contains(&status)
}

/// `"fnv1a64-<16 hex>"`, strong: equal bytes, equal tag, on every process and node.
pub(crate) fn etag_for(body: &[u8]) -> String {
    let mut hasher = Fnv1a::default();
    hasher.update(body);
    format!("\"fnv1a64-{:016x}\"", hasher.finish())
}

/// A header the validators replace: a response that declares its own `ETag` or `Last-Modified`
/// and `_rift.conditional` serves the generated ones.
pub(crate) fn is_validator(name: &str) -> bool {
    name.eq_ignore_ascii_case("etag") || name.eq_ignore_ascii_case("last-modified")
}

/// A configured header a 304 repeats.
pub(crate) fn kept_on_304(name: &str) -> bool {
    KEPT_ON_304
        .iter()
        .any(|kept| name.eq_ignore_ascii_case(kept))
}

/// The preferred HTTP-date form (RFC 7231 §7.1.1.1), as seconds since the epoch. The only form a
/// fixed `lastModified` may take, since it is served verbatim.
pub(crate) fn parse_imf_fixdate(value: &str) -> Option<i64> {
    parse_with(value, IMF_FIXDATE)
}

/// Any of the three HTTP-date forms a recipient must accept (RFC 7231 §7.1.1.1).
fn parse_http_date(value: &str) -> Option<i64> {
    let value = value.trim();
    parse_imf_fixdate(value)
        // RFC 850
        .or_else(|| parse_with(value, "%A, %d-%b-%y %H:%M:%S GMT"))
        // asctime
        .or_else(|| parse_with(value, "%a %b %e %H:%M:%S %Y"))
}

fn parse_with(value: &str, format: &str) -> Option<i64> {
    NaiveDateTime::parse_from_str(value, format)
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// The validators of one response, and the comparison with a request's preconditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Validators {
    etag: Option<String>,
    last_modified: String,
    last_modified_secs: Option<i64>,
}

impl Validators {
    /// Whether the request's preconditions say the client already holds this representation. An
    /// `If-None-Match` decides alone; `If-Modified-Since` is read only without one (RFC 7232 §6).
    pub(crate) fn not_modified(&self, request_headers: &FastMap<String, Vec<String>>) -> bool {
        let mut if_none_match = header_values(request_headers, "if-none-match").peekable();
        if if_none_match.peek().is_some() {
            return if_none_match.any(|list| self.none_match_hits(list));
        }
        let Some(last_modified) = self.last_modified_secs else {
            return false;
        };
        // A list is not a date, so only the first line is read; an unparseable one is ignored.
        header_values(request_headers, "if-modified-since")
            .next()
            .and_then(parse_http_date)
            .is_some_and(|since| last_modified <= since)
    }

    /// Weak comparison (RFC 7232 §2.3.2): a `W/` prefix on either side is ignored.
    fn none_match_hits(&self, list: &str) -> bool {
        if list.trim() == "*" {
            return true;
        }
        let Some(etag) = self.etag.as_deref() else {
            return false;
        };
        entity_tags(list).any(|tag| tag.strip_prefix("W/").unwrap_or(tag) == etag)
    }

    /// The validator headers a 2xx carries.
    pub(crate) fn headers(&self) -> impl Iterator<Item = (HeaderName, &str)> {
        self.etag
            .as_deref()
            .map(|etag| (ETAG, etag))
            .into_iter()
            .chain(std::iter::once((
                LAST_MODIFIED,
                self.last_modified.as_str(),
            )))
    }

    /// The 304: the validators, `kept` (the configured headers [`kept_on_304`] selects) and the
    /// imposter marker, with no body, `Content-Type` or `Content-Length`. Errs only on a `kept` or
    /// fixed `Last-Modified` value that is not a valid header, which the full response would fail
    /// on too.
    pub(crate) fn not_modified_response<K, V>(
        &self,
        kept: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Response<Full<Bytes>>, hyper::http::Error>
    where
        K: TryInto<HeaderName>,
        <K as TryInto<HeaderName>>::Error: Into<hyper::http::Error>,
        V: TryInto<hyper::header::HeaderValue>,
        <V as TryInto<hyper::header::HeaderValue>>::Error: Into<hyper::http::Error>,
    {
        let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
        for (name, value) in kept {
            builder = builder.header(name, value);
        }
        for (name, value) in self.headers() {
            builder = builder.header(name, value);
        }
        builder
            .header("x-rift-imposter", "true")
            .body(Full::new(Bytes::new()))
    }
}

/// Every value of the header `name`, whatever casing the request map keeps its names in.
fn header_values<'a>(
    headers: &'a FastMap<String, Vec<String>>,
    name: &'a str,
) -> impl Iterator<Item = &'a str> {
    headers
        .iter()
        .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
        .flat_map(|(_, values)| values.iter().map(String::as_str))
}

/// The entity tags of an `If-None-Match` list, each with its `W/` prefix and quotes. A tag may hold
/// a comma, so the list is scanned rather than split. Stops at the first malformed entry.
fn entity_tags(list: &str) -> impl Iterator<Item = &str> {
    let mut rest = list;
    std::iter::from_fn(move || {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_ascii_whitespace());
        let prefix = if rest.starts_with("W/") { 2 } else { 0 };
        let quoted = rest.get(prefix..)?.strip_prefix('"')?;
        let end = prefix + 1 + quoted.find('"')? + 1;
        let (tag, tail) = rest.split_at(end);
        rest = tail;
        Some(tag)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = "\"fnv1a64-a430d84680aabd0b\"";

    fn request(pairs: &[(&str, &str)]) -> FastMap<String, Vec<String>> {
        let mut headers: FastMap<String, Vec<String>> = FastMap::default();
        for (name, value) in pairs {
            headers
                .entry((*name).to_owned())
                .or_default()
                .push((*value).to_owned());
        }
        headers
    }

    fn fixed(etag: bool, date: &str) -> ConditionalSpec {
        ConditionalSpec::from_config(Some(&ConditionalGet::Validators(
            crate::imposter::types::ConditionalValidators {
                etag: Some(etag),
                last_modified: Some(LastModified::Fixed(date.to_owned())),
            },
        )))
        .expect("enabled")
    }

    fn hello_validators() -> Validators {
        fixed(true, "Sat, 03 Oct 2026 12:00:00 GMT").validators(Some(HELLO.to_owned()), Utc::now())
    }

    #[test]
    fn etag_is_fnv1a64_of_the_bytes() {
        assert_eq!(etag_for(b"hello"), HELLO);
        assert_eq!(etag_for(b""), "\"fnv1a64-cbf29ce484222325\"");
    }

    #[test]
    fn false_and_absent_are_off_and_true_is_etag_plus_load() {
        assert_eq!(ConditionalSpec::from_config(None), None);
        assert_eq!(
            ConditionalSpec::from_config(Some(&ConditionalGet::Enabled(false))),
            None
        );
        assert_eq!(
            ConditionalSpec::from_config(Some(&ConditionalGet::Enabled(true))),
            Some(ConditionalSpec {
                etag: true,
                last_modified: Stamp::Load
            })
        );
    }

    #[test]
    fn load_stamp_is_an_imf_fixdate_of_the_load_time() {
        let spec = ConditionalSpec::from_config(Some(&ConditionalGet::Enabled(true))).expect("on");
        let loaded = DateTime::from_timestamp(1_791_028_800, 0).expect("valid");
        let validators = spec.validators(None, loaded);
        assert_eq!(validators.last_modified, "Sat, 03 Oct 2026 12:00:00 GMT");
        assert_eq!(validators.last_modified_secs, Some(1_791_028_800));
    }

    #[test]
    fn the_three_http_date_forms_parse() {
        for date in [
            "Sat, 03 Oct 2026 12:00:00 GMT",
            "Saturday, 03-Oct-26 12:00:00 GMT",
            "Sat Oct  3 12:00:00 2026",
        ] {
            assert_eq!(parse_http_date(date), Some(1_791_028_800), "{date}");
        }
        assert_eq!(parse_imf_fixdate("Saturday, 03-Oct-26 12:00:00 GMT"), None);
        assert_eq!(parse_imf_fixdate("yesterday"), None);
    }

    #[test]
    fn entity_tags_scan_quoted_commas_and_weak_prefixes() {
        let tags: Vec<&str> = entity_tags(r#" "a,b" ,W/"c",  "d""#).collect();
        assert_eq!(tags, [r#""a,b""#, r#"W/"c""#, r#""d""#]);
        assert_eq!(entity_tags("unquoted").count(), 0);
    }

    #[test]
    fn if_none_match_matches_strong_weak_listed_and_star() {
        let v = hello_validators();
        for value in [
            HELLO,
            "W/\"fnv1a64-a430d84680aabd0b\"",
            "\"x\", \"fnv1a64-a430d84680aabd0b\"",
            "*",
        ] {
            assert!(
                v.not_modified(&request(&[("If-None-Match", value)])),
                "{value}"
            );
        }
        assert!(!v.not_modified(&request(&[("if-none-match", "\"x\"")])));
    }

    #[test]
    fn if_none_match_across_two_header_lines() {
        let v = hello_validators();
        assert!(v.not_modified(&request(&[
            ("If-None-Match", "\"x\""),
            ("If-None-Match", HELLO)
        ])));
    }

    #[test]
    fn if_none_match_decides_alone() {
        let v = hello_validators();
        assert!(!v.not_modified(&request(&[
            ("If-None-Match", "\"x\""),
            ("If-Modified-Since", "Sat, 01 Jan 2050 00:00:00 GMT")
        ])));
    }

    #[test]
    fn if_modified_since_compares_at_second_granularity() {
        let v = hello_validators();
        let at = |date: &str| v.not_modified(&request(&[("If-Modified-Since", date)]));
        assert!(at("Sat, 03 Oct 2026 12:00:00 GMT"));
        assert!(at("Sat, 03 Oct 2026 12:00:01 GMT"));
        assert!(!at("Sat, 03 Oct 2026 11:59:59 GMT"));
        assert!(!at("garbage"));
    }

    #[test]
    fn without_an_etag_only_star_matches() {
        let v = fixed(false, "Sat, 03 Oct 2026 12:00:00 GMT").validators(None, Utc::now());
        assert!(!v.not_modified(&request(&[("If-None-Match", HELLO)])));
        assert!(v.not_modified(&request(&[("If-None-Match", "*")])));
    }

    #[test]
    fn an_unparseable_fixed_date_never_answers_304_by_date() {
        let v = fixed(true, "yesterday").validators(None, Utc::now());
        assert!(!v.not_modified(&request(&[(
            "If-Modified-Since",
            "Sat, 01 Jan 2050 00:00:00 GMT"
        )])));
    }

    #[test]
    fn only_get_and_head_with_a_2xx() {
        assert!(applies("GET", 200));
        assert!(applies("HEAD", 204));
        assert!(!applies("POST", 200));
        assert!(!applies("GET", 404));
        assert!(!applies("GET", 302));
    }

    #[test]
    fn the_304_carries_validators_and_kept_headers_only() {
        let response = hello_validators()
            .not_modified_response([("Cache-Control", "max-age=60")])
            .expect("valid headers");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let headers = response.headers();
        assert_eq!(headers["etag"], HELLO);
        assert_eq!(headers["last-modified"], "Sat, 03 Oct 2026 12:00:00 GMT");
        assert_eq!(headers["cache-control"], "max-age=60");
        assert_eq!(headers["x-rift-imposter"], "true");
        assert_eq!(headers.len(), 4);
    }
}
