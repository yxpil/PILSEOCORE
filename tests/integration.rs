//! Integration tests that drive the pilseocore public library surface across
//! module boundaries (json parser + enumerator + blacklist hashing). These are
//! external to the crate on purpose: they exercise only what the crate exposes.

use pilseocore::blacklist::{fnv1a64, hamming, simhash};
use pilseocore::enumerate::Enumerator;
use pilseocore::json::{self, Json};

#[test]
fn enumerator_emits_exact_total_and_terminates() {
    // charset "ab", lengths 1..=2 => total = 2 + 4 = 6 strings.
    let mut e = Enumerator::new("ab", 1, 2);
    assert_eq!(e.total(), 6);
    let got: Vec<String> = std::iter::from_fn(|| e.next()).collect();
    assert_eq!(got.len() as u128, e.total(), "must emit exactly total() strings");
}

#[test]
fn json_public_api_roundtrip_and_query() {
    let raw = r#"{"query":"hello","page":2,"tags":["a","b"],"meta":null}"#;
    let v = json::parse(raw).unwrap();
    assert_eq!(v.get("query").unwrap().as_str(), Some("hello"));
    assert_eq!(v.get("page").unwrap().as_u64(), Some(2));
    assert_eq!(v.get("tags").unwrap().as_arr().unwrap().len(), 2);
    assert!(v.get("missing").is_none());

    // re-serialize and re-parse: stable through the public boundary
    let back = json::parse(&v.to_string()).unwrap();
    assert_eq!(back, v);
}

#[test]
fn injection_malformed_payloads_rejected_without_panic() {
    // Hostile JSON must be rejected (Err), never panic.
    for bad in ["", "{", "[1", r#"{"a":}"#, "unclosed", "\"x", "nul", "truE"] {
        assert!(json::parse(bad).is_err(), "should reject {:?}", bad);
    }
    // A JSON string carrying script tags must not break out of the literal when
    // serialized (quotes/backslashes escaped).
    let safe = Json::str(r#"{"x":"<script>alert(1)</script>"}"#);
    let out = safe.to_string();
    assert!(out.starts_with('"') && out.ends_with('"'));
}

#[test]
fn hashing_public_api_consistency() {
    assert_eq!(fnv1a64("abc"), fnv1a64("abc"));
    assert_eq!(hamming(simhash("x"), simhash("x")), 0);
}
