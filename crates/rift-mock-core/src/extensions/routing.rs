//! Host-matching helper shared with the front door.
//!
//! This module used to hold `Router` — the reverse-proxy request router — which went with the
//! `Config` mode in #975. `is_subdomain_of` stays because `rift-http-proxy`'s front-door route
//! table matches hosts by the same rule, and two copies of "what does `*.` mean" is precisely how
//! the label-boundary bug it encodes would come back.

/// Public because the front-door route table (`rift-http-proxy`) matches hosts by
/// the same rule, and two copies of "what does `*.` mean" is precisely how the
/// label-boundary bug this encodes would come back.
///
/// True only for an actual subdomain: there must be at least one non-empty label
/// ending at a literal `.` boundary. The previous `host.ends_with(suffix)` also
/// accepted `example.com` (no label at all) and `evilexample.com` (no boundary) —
/// the latter routing an attacker-chosen host to whatever upstream the route names.
pub fn is_subdomain_of(host: &str, suffix: &str) -> bool {
    let Some(cut) = host.len().checked_sub(suffix.len()) else {
        return false;
    };
    if !host.is_char_boundary(cut) {
        return false;
    }
    let (label, tail) = host.split_at(cut);
    tail.eq_ignore_ascii_case(suffix) && label.len() > 1 && label.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomain_requires_a_real_label_and_a_dot_boundary() {
        assert!(is_subdomain_of("api.example.com", "example.com"));
        assert!(is_subdomain_of("a.b.example.com", "example.com"));
        // The bug this function exists to prevent: `ends_with` accepted both of these.
        assert!(
            !is_subdomain_of("example.com", "example.com"),
            "the apex is not a subdomain of itself"
        );
        assert!(
            !is_subdomain_of("evilexample.com", "example.com"),
            "no dot boundary — this routed an attacker-chosen host to the named upstream"
        );
        assert!(
            !is_subdomain_of("com", "example.com"),
            "shorter than the suffix"
        );
        assert!(
            is_subdomain_of("API.Example.COM", "example.com"),
            "hostnames are case-insensitive (RFC 4343)"
        );
    }

    #[test]
    fn subdomain_does_not_panic_when_the_cut_lands_inside_a_multibyte_char() {
        // The cut is `host.len() - suffix.len()` in BYTES, so it must land strictly inside a
        // multibyte char for `is_char_boundary` to be doing anything. "hé.com" is 7 bytes
        // (é occupies 1..=2) and the suffix is 5, so the cut is 2 — inside é. Without the guard
        // `split_at` panics here; with it the answer is a plain `false`.
        assert!(!is_subdomain_of("hé.com", "x.com"));
        // A cut that lands on a valid boundary just after a multibyte char still answers normally.
        assert!(!is_subdomain_of("héllo.com", "llo.com"));
    }
}
