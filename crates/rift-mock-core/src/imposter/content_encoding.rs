//! Decoding a `gzip` / `br` request body before it is matched and recorded, as Mountebank's
//! `httpRequest.js` does (issue #1584).
//!
//! Mountebank decodes only when the `Content-Encoding` value is exactly `gzip` or `br`; any other
//! value (`GZIP`, `gzip, br`, `deflate`) leaves the bytes as sent. A body that does not decode has
//! no body at all: Mountebank catches the decoder's error and leaves `request.body` undefined.
//!
//! Mountebank decodes without a bound. Rift streams the decoded bytes into a collector of the same
//! [`BodyGuard`](super::body_limits::BodyGuard) the raw body came through, so the decoded size is
//! what `--max-body-size` limits and it is charged to the shared memory budget like a raw body of
//! that size (#1439): a small bomb is a `413`, never an allocation.

use super::body_limits::{BodyCollector, BodyReadError, CollectedBody};
use std::io::Read;

/// A request-body coding Mountebank decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coding {
    Gzip,
    Brotli,
}

impl Coding {
    /// The coding of a request, from its first `Content-Encoding` value compared exactly, as
    /// Mountebank compares it. `None` is a body to take as sent.
    #[must_use]
    pub(crate) fn of(headers: &hyper::HeaderMap) -> Option<Self> {
        match headers.get(hyper::header::CONTENT_ENCODING)?.as_bytes() {
            b"gzip" => Some(Self::Gzip),
            b"br" => Some(Self::Brotli),
            _ => None,
        }
    }
}

/// What decoding a body produced.
#[derive(Debug)]
pub(crate) enum Decoded {
    /// The decoded bytes, holding their share of the body budget.
    Body(CollectedBody),
    /// The bytes do not decode as the coding says: Mountebank's `undefined` body.
    Undefined,
}

/// Decode `raw` into `out`. Blocking (it is CPU work proportional to the decoded size), so the
/// handler runs it off the async worker.
///
/// `Err` is only a limit refusal from `out`: over the ceiling, or the budget exhausted. A stream
/// that is corrupt, truncated or empty is not an error but [`Decoded::Undefined`] — Mountebank's
/// `gunzipSync`/`brotliDecompressSync` throw on all three, and it catches that as "no body".
pub(crate) fn decode(
    coding: Coding,
    raw: &[u8],
    mut out: BodyCollector,
) -> Result<Decoded, BodyReadError> {
    if raw.is_empty() {
        return Ok(Decoded::Undefined);
    }
    let mut reader: Box<dyn Read + '_> = match coding {
        // Multi-member, as Node's gunzip concatenates every member. Trailing bytes that are not a
        // gzip member fail the decode, as they do in Node.
        Coding::Gzip => Box::new(flate2::read::MultiGzDecoder::new(raw)),
        // The reader's first `Ok(0)` is the end of the brotli stream; bytes after it are never
        // looked at, as Node ignores them.
        Coding::Brotli => Box::new(brotli_decompressor::Decompressor::new(raw, 4096)),
    };
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(Decoded::Body(out.finish())),
            Ok(n) => out.push(&chunk[..n])?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Domain value, not a swallowed failure: a body that does not decode is Mountebank's
            // undefined body, which every consumer then sees as missing.
            Err(_) => return Ok(Decoded::Undefined),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::body_limits::{BodyGuard, BodyLimits};
    use std::io::Write;

    /// `brotliCompressSync("plain text")`, from Node 22's zlib.
    const BR_PLAIN_TEXT: &[u8] = &[
        0x8b, 0x04, 0x80, 0x70, 0x6c, 0x61, 0x69, 0x6e, 0x20, 0x74, 0x65, 0x78, 0x74, 0x03,
    ];

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).expect("gzip into a Vec");
        encoder.finish().expect("gzip into a Vec")
    }

    fn guard(max: usize) -> BodyGuard {
        BodyGuard::new(BodyLimits::new(max, Some(max)).expect("valid limits"))
    }

    async fn run(guard: &BodyGuard, coding: Coding, raw: &[u8]) -> Result<Decoded, BodyReadError> {
        decode(coding, raw, guard.start(None).await.expect("start"))
    }

    fn text(decoded: Result<Decoded, BodyReadError>) -> Option<String> {
        match decoded.expect("not refused") {
            Decoded::Body(body) => Some(String::from_utf8(body.to_vec()).expect("utf-8")),
            Decoded::Undefined => None,
        }
    }

    fn headers(encoding: &str) -> hyper::HeaderMap {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_ENCODING,
            encoding.parse().expect("header value"),
        );
        headers
    }

    #[test]
    fn only_exactly_gzip_or_br_is_decoded() {
        assert_eq!(Coding::of(&headers("gzip")), Some(Coding::Gzip));
        assert_eq!(Coding::of(&headers("br")), Some(Coding::Brotli));
        for raw in [
            "GZIP", "Br", "gzip, br", "deflate", "x-gzip", "identity", "",
        ] {
            assert_eq!(Coding::of(&headers(raw)), None, "{raw:?} is taken as sent");
        }
        assert_eq!(Coding::of(&hyper::HeaderMap::new()), None);
    }

    #[test]
    fn the_first_content_encoding_value_decides() {
        let mut map = headers("gzip");
        map.append(
            hyper::header::CONTENT_ENCODING,
            hyper::header::HeaderValue::from_static("br"),
        );
        assert_eq!(Coding::of(&map), Some(Coding::Gzip));
    }

    #[tokio::test]
    async fn a_valid_stream_decodes_to_its_text() {
        let g = guard(1024);
        assert_eq!(
            text(run(&g, Coding::Gzip, &gzip(b"plain text")).await).as_deref(),
            Some("plain text")
        );
        assert_eq!(
            text(run(&g, Coding::Brotli, BR_PLAIN_TEXT).await).as_deref(),
            Some("plain text")
        );
    }

    #[tokio::test]
    async fn an_invalid_truncated_or_empty_stream_is_undefined() {
        let g = guard(1024);
        let valid = gzip(b"plain text");
        for coding in [Coding::Gzip, Coding::Brotli] {
            assert_eq!(
                text(run(&g, coding, b"plain text").await),
                None,
                "{coding:?}"
            );
            assert_eq!(text(run(&g, coding, b"").await), None, "{coding:?} empty");
        }
        assert_eq!(
            text(run(&g, Coding::Gzip, &valid[..valid.len() - 4]).await),
            None
        );
        assert_eq!(
            text(
                run(
                    &g,
                    Coding::Brotli,
                    &BR_PLAIN_TEXT[..BR_PLAIN_TEXT.len() - 2]
                )
                .await
            ),
            None
        );
    }

    /// What Node's zlib does past the end of a stream: gzip reads every member and refuses other
    /// trailing bytes; brotli ignores what follows its stream.
    #[tokio::test]
    async fn trailing_bytes_are_treated_as_node_treats_them() {
        let g = guard(1024);
        let one = gzip(b"plain text");
        let two = [one.as_slice(), one.as_slice()].concat();
        assert_eq!(
            text(run(&g, Coding::Gzip, &two).await).as_deref(),
            Some("plain textplain text")
        );
        let garbage = [one.as_slice(), b"xyz"].concat();
        assert_eq!(text(run(&g, Coding::Gzip, &garbage).await), None);
        let garbage = [BR_PLAIN_TEXT, b"xyz"].concat();
        assert_eq!(
            text(run(&g, Coding::Brotli, &garbage).await).as_deref(),
            Some("plain text")
        );
    }

    /// A 1 MiB inflation from about 1 KiB is refused at the ceiling, and the budget the partial
    /// output held is released with it.
    #[tokio::test]
    async fn a_bomb_is_refused_at_the_ceiling_and_releases_the_budget() {
        let g = guard(64 * 1024);
        let bomb = gzip(&vec![0u8; 1024 * 1024]);
        assert!(bomb.len() < 4 * 1024, "{}", bomb.len());
        assert!(matches!(
            run(&g, Coding::Gzip, &bomb).await,
            Err(BodyReadError::TooLarge { limit }) if limit == 64 * 1024
        ));
        let held = match run(&g, Coding::Gzip, &gzip(&vec![b'a'; 64 * 1024])).await {
            Ok(Decoded::Body(body)) => body,
            other => panic!("the ceiling itself decodes: {other:?}"),
        };
        assert_eq!(held.len(), 64 * 1024);
        // The decoded body holds the whole budget while it lives…
        assert!(matches!(
            run(&g, Coding::Gzip, &gzip(b"y")).await,
            Err(BodyReadError::BudgetExhausted)
        ));
        drop(held);
        // …and gives it all back.
        assert!(matches!(
            run(&g, Coding::Gzip, &gzip(&vec![b'a'; 64 * 1024])).await,
            Ok(Decoded::Body(_))
        ));
    }
}
