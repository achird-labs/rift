//! Response text that remembers which bytes the engine substituted into it (issue #1203).
//!
//! Every substitution pass — `{{ }}` templating, `${request.*}`, `copy`, `lookup` — searches the
//! output of the pass before it. Without provenance none of them can tell the author's text from
//! text an earlier pass spliced in from the request, so a client that sends a complete token gets
//! it expanded: a lookup column the config never served, or a request header the client never saw.
//!
//! The rule: **a token is expanded only if the author wrote its first and last characters.** Text
//! the engine substituted is never read as a token on its own, and it cannot complete one either:
//! a client that sends `${request.headers.x` gets nothing from an authored `}` that follows it, and
//! one that sends `{request.headers.x}` gets nothing from an authored `$` before it. Only the
//! middle may be substituted, which keeps the one deliberate composition working — `${R}[${COL}]`
//! with a `copy` into `${COL}` and a `lookup` into `${R}`: the delimiters are the author's.

use regex::{Captures, Regex};
use std::collections::HashMap;
use std::ops::Range;

/// Response text plus the byte ranges the engine substituted into it.
///
/// Invariant: `inserted` is sorted, non-empty, non-overlapping and non-adjacent (neighbours are
/// merged), and every bound lies on a char boundary of `text`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Spliced {
    text: String,
    inserted: Vec<Range<usize>>,
}

/// Header values with provenance, one entry per header name and one element per header line.
pub(crate) type SplicedHeaders = HashMap<String, Vec<Spliced>>;

impl Spliced {
    /// Text the author wrote (or that is treated as theirs: a script's output, a proxied body).
    /// Does not allocate beyond `text`.
    pub(crate) fn authored(text: String) -> Self {
        Self {
            text,
            inserted: Vec::new(),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn into_text(self) -> String {
        self.text
    }

    #[cfg(test)]
    fn inserted(&self) -> &[Range<usize>] {
        &self.inserted
    }

    /// Whether the byte at `pos` is text the engine inserted.
    fn byte_inserted(&self, pos: usize) -> bool {
        // The last range starting at or before `pos` is the only one that can contain it.
        let idx = self.inserted.partition_point(|r| r.start <= pos);
        idx > 0 && pos < self.inserted[idx - 1].end
    }

    /// Whether the non-empty match `span` is a token the author wrote: its first and last bytes
    /// are both authored.
    fn is_authored_token(&self, span: &Range<usize>) -> bool {
        !self.byte_inserted(span.start) && !self.byte_inserted(span.end - 1)
    }

    /// Whether `token` occurs anywhere as a token the author wrote.
    pub(crate) fn contains_authored(&self, token: &str) -> bool {
        !token.is_empty()
            && self
                .text
                .match_indices(token)
                .any(|(at, _)| self.is_authored_token(&(at..at + token.len())))
    }

    /// Replace every occurrence of `token` whose first and last bytes are authored; the
    /// replacement is recorded as inserted. One left-to-right pass over the current text, so a
    /// replacement is never itself searched. Returns the number of replacements.
    pub(crate) fn replace_token(&mut self, token: &str, replacement: &str) -> usize {
        if token.is_empty() {
            return 0;
        }
        let matches: Vec<(Range<usize>, String)> = self
            .text
            .match_indices(token)
            .map(|(at, _)| at..at + token.len())
            .filter(|span| self.is_authored_token(span))
            .map(|span| (span, replacement.to_string()))
            .collect();
        self.splice(matches)
    }

    /// [`Self::replace_token`] for a regex pass: `replacement` receives each match's captures and
    /// is called only for matches whose first and last bytes are authored.
    pub(crate) fn replace_regex(
        &mut self,
        re: &Regex,
        mut replacement: impl FnMut(&Captures) -> String,
    ) -> usize {
        let mut matches = Vec::new();
        for caps in re.captures_iter(&self.text) {
            let Some(whole) = caps.get(0) else { continue };
            let span = whole.range();
            if span.is_empty() || !self.is_authored_token(&span) {
                continue;
            }
            matches.push((span, replacement(&caps)));
        }
        self.splice(matches)
    }

    /// Apply sorted, non-overlapping `(span, replacement)` pairs, carrying the existing inserted
    /// ranges through the untouched text and recording each replacement as inserted.
    fn splice(&mut self, matches: Vec<(Range<usize>, String)>) -> usize {
        if matches.is_empty() {
            return 0;
        }
        let count = matches.len();
        let old = std::mem::take(&mut self.text);
        let old_ranges = std::mem::take(&mut self.inserted);
        let mut out = Rebuild {
            text: String::with_capacity(old.len()),
            ranges: Vec::with_capacity(old_ranges.len() + count),
            next_range: 0,
        };
        let mut pos = 0;
        for (span, replacement) in matches {
            out.copy_untouched(&old, &old_ranges, pos..span.start);
            pos = span.end;
            let start = out.text.len();
            out.text.push_str(&replacement);
            if !replacement.is_empty() {
                push_merged(&mut out.ranges, start..out.text.len());
            }
        }
        out.copy_untouched(&old, &old_ranges, pos..old.len());
        self.text = out.text;
        self.inserted = out.ranges;
        count
    }
}

/// The text and ranges [`Spliced::splice`] is building.
struct Rebuild {
    text: String,
    ranges: Vec<Range<usize>>,
    /// First old range not yet wholly behind the cursor.
    next_range: usize,
}

impl Rebuild {
    /// Copy `old[segment]` verbatim, carrying over the parts of old inserted ranges inside it.
    fn copy_untouched(&mut self, old: &str, old_ranges: &[Range<usize>], segment: Range<usize>) {
        let shift = self.text.len();
        while let Some(r) = old_ranges.get(self.next_range) {
            if r.start >= segment.end {
                break;
            }
            let (start, stop) = (r.start.max(segment.start), r.end.min(segment.end));
            if start < stop {
                push_merged(
                    &mut self.ranges,
                    shift + start - segment.start..shift + stop - segment.start,
                );
            }
            if r.end > segment.end {
                // Continues into the match that follows (consumed) or past it (clipped next time).
                break;
            }
            self.next_range += 1;
        }
        self.text.push_str(&old[segment]);
    }
}

fn push_merged(ranges: &mut Vec<Range<usize>>, next: Range<usize>) {
    if let Some(last) = ranges.last_mut()
        && next.start <= last.end
    {
        last.end = last.end.max(next.end);
        return;
    }
    ranges.push(next);
}

/// Every header value as authored text.
pub(crate) fn authored_headers(headers: HashMap<String, Vec<String>>) -> SplicedHeaders {
    headers
        .into_iter()
        .map(|(k, values)| (k, values.into_iter().map(Spliced::authored).collect()))
        .collect()
}

/// Drop provenance: the header map the response builder takes.
pub(crate) fn plain_headers(headers: SplicedHeaders) -> HashMap<String, Vec<String>> {
    headers
        .into_iter()
        .map(|(k, values)| (k, values.into_iter().map(Spliced::into_text).collect()))
        .collect()
}

impl PartialEq<&str> for Spliced {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<str> for Spliced {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

// Range lists here are the inserted-range sets under test, so a one-element list is intended.
#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn with_inserted(text: &str, inserted: &[Range<usize>]) -> Spliced {
        Spliced {
            text: text.to_string(),
            inserted: inserted.to_vec(),
        }
    }

    #[test]
    fn authored_text_is_replaced_and_the_replacement_is_recorded() {
        let mut s = Spliced::authored("a ${T} b ${T}".to_string());
        assert_eq!(s.replace_token("${T}", "xy"), 2);
        assert_eq!(s.as_str(), "a xy b xy");
        assert_eq!(s.inserted(), &[2..4, 7..9]);
    }

    #[test]
    fn a_token_wholly_inside_inserted_text_is_skipped() {
        // "q=" authored, "${R}[secret]" inserted, " name=${R}[name]" authored.
        let mut s = with_inserted("q=${R}[secret] name=${R}[name]", &[2..14]);
        assert!(s.contains_authored("${R}[name]"));
        assert!(!s.contains_authored("${R}[secret]"));
        assert_eq!(s.replace_token("${R}[secret]", "s3cr3t"), 0);
        assert_eq!(s.replace_token("${R}[name]", "alice"), 1);
        assert_eq!(s.as_str(), "q=${R}[secret] name=alice");
        assert_eq!(s.inserted(), &[2..14, 20..25]);
    }

    #[test]
    fn inserted_text_cannot_be_closed_by_an_authored_character() {
        // The client sent "${R}[secret" and the author's "]" follows it.
        let mut s = with_inserted("[${R}[secret]]", &[1..12]);
        assert!(!s.contains_authored("${R}[secret]"));
        assert_eq!(s.replace_token("${R}[secret]", "s3cr3t"), 0);
        assert_eq!(s.as_str(), "[${R}[secret]]");
    }

    #[test]
    fn inserted_text_cannot_be_opened_by_an_authored_character() {
        // The author wrote a literal "$" (a price) and the client sent "{R}[secret]".
        let mut s = with_inserted("${R}[secret]", &[1..12]);
        assert_eq!(s.replace_token("${R}[secret]", "s3cr3t"), 0);
        assert_eq!(s.as_str(), "${R}[secret]");
    }

    #[test]
    fn inserted_text_between_authored_delimiters_is_replaced_with_them() {
        // "${R}[" and "]" authored, "name" inserted: the author opted in to a client-chosen column.
        let mut s = with_inserted("<${R}[name]>", &[6..10]);
        assert!(s.contains_authored("${R}[name]"));
        assert_eq!(s.replace_token("${R}[name]", "alice"), 1);
        assert_eq!(s.as_str(), "<alice>");
        assert_eq!(s.inserted(), &[1..6]);
    }

    #[test]
    fn an_empty_replacement_records_no_range() {
        let mut s = Spliced::authored("a${T}b".to_string());
        assert_eq!(s.replace_token("${T}", ""), 1);
        assert_eq!(s.as_str(), "ab");
        assert!(s.inserted().is_empty());
    }

    #[test]
    fn a_token_replaced_by_itself_terminates_and_is_not_expanded_again() {
        let mut s = Spliced::authored("${T}".to_string());
        assert_eq!(s.replace_token("${T}", "${T}"), 1);
        assert_eq!(s.replace_token("${T}", "x"), 0);
        assert_eq!(s.as_str(), "${T}");
    }

    #[test]
    fn adjacent_replacements_merge_into_one_range() {
        let mut s = Spliced::authored("${A}${B}".to_string());
        s.replace_token("${A}", "${");
        s.replace_token("${B}", "B}");
        // Both halves are inserted, so the token they form together is not the author's.
        assert_eq!(s.as_str(), "${B}");
        assert_eq!(s.inserted(), &[0..4]);
        assert!(!s.contains_authored("${B}"));
    }

    #[test]
    fn a_regex_pass_skips_inserted_matches_without_evaluating_them() {
        let re = Regex::new(r"\$\{(\w+)\}").expect("regex");
        let mut s = with_inserted("${a}${b}", &[4..8]);
        let mut seen = Vec::new();
        let n = s.replace_regex(&re, |caps| {
            seen.push(caps[1].to_string());
            "A".to_string()
        });
        assert_eq!(n, 1);
        assert_eq!(seen, vec!["a"]);
        assert_eq!(s.as_str(), "A${b}");
        assert_eq!(s.inserted(), &[0..5]);
    }

    #[test]
    fn multi_byte_text_keeps_ranges_on_char_boundaries() {
        let mut s = Spliced::authored("é${T}ü".to_string());
        s.replace_token("${T}", "ß");
        assert_eq!(s.as_str(), "éßü");
        assert_eq!(s.inserted(), &[2..4]);
        assert!(s.as_str().is_char_boundary(2) && s.as_str().is_char_boundary(4));
    }

    fn invariant_holds(s: &Spliced) -> bool {
        s.inserted.iter().all(|r| {
            r.start < r.end
                && r.end <= s.text.len()
                && s.text.is_char_boundary(r.start)
                && s.text.is_char_boundary(r.end)
        }) && s.inserted.windows(2).all(|w| w[0].end < w[1].start)
    }

    /// A seeded xorshift generator: the properties below run a few thousand fixed cases without a
    /// property-testing dependency, and a failure reproduces from its printed case.
    struct Gen(u64);

    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn string(&mut self, alphabet: &[&str], min: usize, max: usize) -> String {
            let len = min + self.below(max - min + 1);
            (0..len)
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }
    }

    const TEXT: &[&str] = &["a", "b", "c", "$", "é", "{", "}"];

    #[test]
    fn with_nothing_inserted_replace_token_is_str_replace() {
        let mut g = Gen(0x1203);
        for _ in 0..4000 {
            let text = g.string(TEXT, 0, 24);
            let token = g.string(TEXT, 1, 3);
            let replacement = g.string(&["x", "y", "ü"], 0, 3);
            let mut s = Spliced::authored(text.clone());
            s.replace_token(&token, &replacement);
            assert_eq!(
                s.as_str(),
                text.replace(&token, &replacement),
                "{text:?} {token:?} {replacement:?}"
            );
        }
    }

    #[test]
    fn the_invariant_survives_any_sequence_of_passes() {
        let mut g = Gen(0xBEEF);
        for _ in 0..3000 {
            let mut s = Spliced::authored(g.string(TEXT, 0, 24));
            for _ in 0..g.below(6) {
                let token = g.string(TEXT, 1, 3);
                let replacement = g.string(&["a", "c", "ü", "$"], 0, 4);
                let before = s.clone();
                s.replace_token(&token, &replacement);
                assert!(
                    invariant_holds(&s),
                    "{s:?} after {before:?} {token:?}->{replacement:?}"
                );
            }
        }
    }

    #[test]
    fn inserted_text_that_survives_a_pass_is_unchanged() {
        let mut g = Gen(0xC0FFEE);
        for _ in 0..4000 {
            let prefix = g.string(&["a", "b", "c"], 0, 6);
            let inserted = g.string(&["a", "b", "c", "$"], 1, 8);
            let suffix = g.string(&["a", "b", "c"], 0, 6);
            let token = g.string(&["a", "b", "c", "$"], 1, 2);
            let text = format!("{prefix}{inserted}{suffix}");
            let span = prefix.len()..prefix.len() + inserted.len();
            let mut s = with_inserted(&text, std::slice::from_ref(&span));
            s.replace_token(&token, "#");
            // Only a match whose first and last bytes are both authored is replaced, and such a
            // match touches the span only by covering it whole. Otherwise the span survives.
            let covers = text
                .match_indices(&token)
                .any(|(at, _)| at < span.start && at + token.len() > span.end);
            if !covers {
                assert!(
                    s.as_str().contains(inserted.as_str()),
                    "{text:?} {span:?} {token:?} -> {s:?}"
                );
            }
        }
    }
}
