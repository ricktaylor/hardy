use std::{cmp::Ordering, collections::BTreeSet};

use hardy_bpv7::eid::{Eid, IpnNodeId};
use hardy_eid_patterns::{EidPattern, EidPatternItem, Error};

// Parses `pattern` and reports whether it matches `eid`, panicking with the
// offending input if either fails to parse.
fn pattern_matches(pattern: &str, eid: &str) -> bool {
    pattern
        .parse::<EidPattern>()
        .unwrap_or_else(|e| panic!("failed to parse pattern {pattern}: {e}"))
        .matches(
            &eid.parse()
                .unwrap_or_else(|e| panic!("failed to parse EID {eid}: {e}")),
        )
}

// Parses `s` and returns its specificity score.
fn score(s: &str) -> Option<u32> {
    s.parse::<EidPattern>()
        .expect("Failed to parse")
        .specificity_score()
}

// Parses both patterns and checks the subset relationship.
fn is_subset(lhs: &str, rhs: &str) -> bool {
    let lhs_pattern: EidPattern = lhs.parse().expect("Failed to parse lhs");
    let rhs_pattern: EidPattern = rhs.parse().expect("Failed to parse rhs");
    lhs_pattern.is_subset(&rhs_pattern)
}

// R-11: a numeric scheme whose first digit is 9 must parse (grammar is
// %x31-39 inclusive); '1'..'9' would exclude it.
#[test]
fn scheme_beginning_with_nine_parses() {
    for s in ["9:**", "91:**", "900:**"] {
        let pat: EidPattern = s
            .parse()
            .unwrap_or_else(|e| panic!("{s} should parse: {e}"));
        assert_eq!(
            pat,
            EidPattern::Set(
                [EidPatternItem::AnyNumericScheme(
                    s.strip_suffix(":**")
                        .expect("test scheme strings end in :**")
                        .parse()
                        .unwrap()
                )]
                .into()
            )
        );
    }
}

// R-12: a scheme-family wildcard must match an unknown-scheme EID (the RIB
// routing table relies on this).
#[test]
fn scheme_wildcard_matches_unknown_scheme_eid() {
    let pat: EidPattern = "88:**".parse().unwrap();
    assert!(pat.matches(&Eid::Unknown {
        scheme: 88,
        data: Box::default(),
    }));
    assert!(!pat.matches(&Eid::Unknown {
        scheme: 89,
        data: Box::default(),
    }));
}

// A text scheme with no numeric code (anything but `dtn`/`ipn`) has no
// representable EID, so `scheme:**` matches nothing: in particular it must not
// match the null endpoint, whose numeric scheme is also `None`.
#[test]
fn unknown_text_scheme_matches_nothing() {
    let pat: EidPattern = "foo:**".parse().unwrap();
    assert!(!pat.matches(&Eid::Null));
    assert!(!pat.matches(&Eid::Unknown {
        scheme: 88,
        data: Box::default(),
    }));
}

// R-21: a multi-item union must sort as more specific than the `*:**` default
// so it can override broader routes at the same priority.
#[test]
fn union_sorts_more_specific_than_any() {
    let union: EidPattern = "ipn:0.5.*|ipn:0.6.*".parse().unwrap();
    let any: EidPattern = "*:**".parse().unwrap();

    assert_eq!(
        union.specificity_score(),
        "ipn:0.5.*"
            .parse::<EidPattern>()
            .unwrap()
            .specificity_score(),
        "a union scores as its broadest member, not None"
    );
    // Ord: more specific compares Less (sorted first in the RIB BTreeMap).
    assert!(
        union < any,
        "union route must order before the default route"
    );
}

// The `*:**` catch-all must match every EID of every scheme: it is the
// default-route pattern the RIB relies on.
#[test]
fn any_pattern_matches_every_eid() {
    let any: EidPattern = "*:**".parse().expect("Failed to parse");
    assert_eq!(any, EidPattern::Any);

    assert!(any.matches(&"ipn:1.2.3".parse().unwrap()));
    assert!(any.matches(&"dtn://node/svc".parse().unwrap()));
    assert!(any.matches(&Eid::Null));
    assert!(any.matches(&Eid::LocalNode(0)));
    assert!(any.matches(&Eid::Unknown {
        scheme: 88,
        data: Box::default(),
    }));
}

#[test]
fn invalid_syntax_rejected() {
    assert!("ipn:1-1".parse::<EidPattern>().is_err());
    assert!("http://*".parse::<EidPattern>().is_err());
    assert!("".parse::<EidPattern>().is_err());
    assert!(":::".parse::<EidPattern>().is_err());
}

// Numeric boundary parsing: the value space is u32 components and u64 scheme
// codes, addressed by textual literals that must reject overflow rather than
// wrap or truncate.
#[test]
fn numeric_boundaries() {
    // u32::MAX is a valid component value.
    assert!("ipn:0.4294967295.1".parse::<EidPattern>().is_ok());
    // u32 overflow must reject, not wrap.
    assert!("ipn:0.4294967296.1".parse::<EidPattern>().is_err());
    // An open range up to u32::MAX matches the top of the value space.
    assert!(pattern_matches(
        "ipn:0.3.[4294967290+]",
        "ipn:0.3.4294967295"
    ));
    // Numeric schemes are non-zero-decimal per the grammar.
    assert!("0:**".parse::<EidPattern>().is_err());
    // u64 overflow of a numeric scheme must reject.
    assert!("99999999999999999999999:**".parse::<EidPattern>().is_err());
}

#[test]
fn ipn_exact() {
    assert!(pattern_matches("ipn:0.3.4", "ipn:0.3.4"));
    assert!(!pattern_matches("ipn:0.3.4", "ipn:0.4.0"));
    assert!(!pattern_matches("ipn:0.3.4", "ipn:0.4.3"));
    assert!(!pattern_matches("ipn:0.3.4", "ipn:1.3.4"));
}

#[test]
fn ipn_legacy_two_element() {
    assert!(pattern_matches("ipn:1.2", "ipn:0.1.2"));
    assert!(!pattern_matches("ipn:1.2", "ipn:0.1.3"));
    assert!(pattern_matches("ipn:1.*", "ipn:0.1.999"));
    assert!(pattern_matches("ipn:*.*", "ipn:0.99.99"));
}

#[test]
fn ipn_service_wildcard() {
    assert!(pattern_matches("ipn:0.3.*", "ipn:0.3.0"));
    assert!(pattern_matches("ipn:0.3.*", "ipn:0.3.4"));
    assert!(pattern_matches("ipn:0.3.*", "ipn:0.3.9999"));
    assert!(!pattern_matches("ipn:0.3.*", "ipn:0.4.3"));
    assert!(!pattern_matches("ipn:0.3.*", "ipn:1.3.3"));
}

#[test]
fn ipn_node_wildcard() {
    assert!(pattern_matches("ipn:0.*.4", "ipn:0.3.4"));
    assert!(pattern_matches("ipn:0.*.4", "ipn:0.999.4"));
    assert!(!pattern_matches("ipn:0.*.4", "ipn:0.3.3"));
    assert!(!pattern_matches("ipn:0.*.4", "ipn:0.3.9999"));
    assert!(!pattern_matches("ipn:0.*.4", "ipn:1.3.4"));
}

#[test]
fn ipn_service_range() {
    assert!(pattern_matches("ipn:0.3.[0-19]", "ipn:0.3.0"));
    assert!(pattern_matches("ipn:0.3.[0-19]", "ipn:0.3.4"));
    assert!(pattern_matches("ipn:0.3.[0-19]", "ipn:0.3.19"));
    assert!(!pattern_matches("ipn:0.3.[0-19]", "ipn:0.3.20"));
    assert!(!pattern_matches("ipn:0.3.[0-19]", "ipn:0.2.19"));

    assert!(pattern_matches("ipn:0.3.[10-19]", "ipn:0.3.10"));
    assert!(pattern_matches("ipn:0.3.[10-19]", "ipn:0.3.15"));
    assert!(pattern_matches("ipn:0.3.[10-19]", "ipn:0.3.19"));
    assert!(!pattern_matches("ipn:0.3.[10-19]", "ipn:0.3.9"));
    assert!(!pattern_matches("ipn:0.3.[10-19]", "ipn:0.2.10"));
    assert!(!pattern_matches("ipn:0.3.[10-19]", "ipn:1.3.10"));
}

#[test]
fn ipn_range_union() {
    for pattern in ["ipn:0.3.[0-4,10-19]", "ipn:0.3.[10-19,0-4]"] {
        assert!(pattern_matches(pattern, "ipn:0.3.0"));
        assert!(pattern_matches(pattern, "ipn:0.3.2"));
        assert!(pattern_matches(pattern, "ipn:0.3.4"));
        assert!(!pattern_matches(pattern, "ipn:0.3.5"));
        assert!(!pattern_matches(pattern, "ipn:0.3.7"));
        assert!(!pattern_matches(pattern, "ipn:0.3.9"));
        assert!(pattern_matches(pattern, "ipn:0.3.10"));
        assert!(pattern_matches(pattern, "ipn:0.3.15"));
        assert!(pattern_matches(pattern, "ipn:0.3.19"));
        assert!(!pattern_matches(pattern, "ipn:0.3.20"));
    }
}

#[test]
fn ipn_range_merge() {
    // Adjacent or overlapping intervals merge into one at parse.
    for pattern in [
        "ipn:0.3.[0-9,10-19]",
        "ipn:0.3.[0-15,10-19]",
        "ipn:0.3.[10-19,0-9]",
    ] {
        assert!(pattern_matches(pattern, "ipn:0.3.0"));
        assert!(pattern_matches(pattern, "ipn:0.3.9"));
        assert!(pattern_matches(pattern, "ipn:0.3.10"));
        assert!(pattern_matches(pattern, "ipn:0.3.19"));
        assert!(!pattern_matches(pattern, "ipn:0.3.20"));
    }
    assert!(pattern_matches("ipn:0.3.[0-15,10-19]", "ipn:0.3.14"));
    assert!(pattern_matches("ipn:0.3.[0-15,10-19]", "ipn:0.3.15"));
    assert!(pattern_matches("ipn:0.3.[0-15,10-19]", "ipn:0.3.16"));
}

#[test]
fn ipn_open_range() {
    assert!(!pattern_matches("ipn:0.3.[10+]", "ipn:0.3.1"));
    assert!(!pattern_matches("ipn:0.3.[10+]", "ipn:0.3.9"));
    assert!(pattern_matches("ipn:0.3.[10+]", "ipn:0.3.10"));
    assert!(pattern_matches("ipn:0.3.[10+]", "ipn:0.3.11"));
    assert!(pattern_matches("ipn:0.3.[10+]", "ipn:0.3.9999"));
}

#[test]
fn ipn_inverted_range_normalised() {
    // Inverted range is normalised by the parser (min/max swap).
    assert!(pattern_matches("ipn:0.3.[10-5]", "ipn:0.3.7"));
    assert!(pattern_matches("ipn:0.3.[10-5]", "ipn:0.3.5"));
    assert!(pattern_matches("ipn:0.3.[10-5]", "ipn:0.3.10"));
    assert!(!pattern_matches("ipn:0.3.[10-5]", "ipn:0.3.4"));
    assert!(!pattern_matches("ipn:0.3.[10-5]", "ipn:0.3.11"));
}

#[test]
fn ipn_bang_local_node() {
    assert!(!pattern_matches("ipn:!.*", "ipn:0.3.1"));
    assert!(pattern_matches("ipn:!.*", "ipn:0.4294967295.0"));
    assert!(pattern_matches("ipn:!.*", "ipn:0.4294967295.1"));
    assert!(pattern_matches("ipn:!.*", "ipn:0.4294967295.999999"));
    assert!(!pattern_matches("ipn:!.*", "ipn:1.4294967295.1"));
}

// `dtn:none` parses to `Eid::Null`, which ipn patterns treat as `ipn:0.0.0`:
// null-endpoint bundles must not escape ipn catch-all filters.
#[test]
fn ipn_patterns_match_null_endpoint() {
    assert!(pattern_matches("ipn:0.0.0", "dtn:none"));
    assert!(pattern_matches("ipn:*.*", "dtn:none"));
    assert!(!pattern_matches("ipn:0.3.*", "dtn:none"));
    assert!(!pattern_matches("ipn:!.*", "dtn:none"));
}

// A single-value range spelling is the same pattern as the bare number, so the
// two parse to equal values and each is a subset of the other.
#[test]
fn single_value_range_equals_bare_number() {
    assert_eq!(
        "ipn:0.3.[5]".parse::<EidPattern>().unwrap(),
        "ipn:0.3.5".parse::<EidPattern>().unwrap()
    );
    assert!(is_subset("ipn:0.3.[5]", "ipn:0.3.5"));
    assert!(is_subset("ipn:0.3.5", "ipn:0.3.[5]"));
}

// Display output must re-parse to an equal pattern: RIB map keys and serde
// round-trips (serialized as strings) both depend on it.
#[test]
fn display_parse_roundtrip() {
    let corpus = [
        "ipn:0.3.4",
        "ipn:0.3.[5]",
        "ipn:0.3.[10+]",
        "ipn:!.*",
        "ipn:0.3.[0-4,10-19]",
        "*:**",
        "88:**",
        "ipn:0.3.4|ipn:0.5.*",
        #[cfg(feature = "dtn-pat-item")]
        "dtn://node/svc",
        #[cfg(feature = "dtn-pat-item")]
        "dtn://node/**",
        #[cfg(feature = "dtn-pat-item")]
        "dtn:none",
    ];
    for s in corpus {
        let pattern: EidPattern = s
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse pattern {s}: {e}"));
        let displayed = pattern.to_string();
        let reparsed: EidPattern = displayed
            .parse()
            .unwrap_or_else(|e| panic!("failed to re-parse {displayed} (from {s}): {e}"));
        assert_eq!(reparsed, pattern, "{s} -> {displayed} did not round-trip");
    }
}

// Converting an exact Eid to a pattern and back must yield the original,
// canonical Eid (including the Null and LocalNode special forms).
#[test]
fn eid_pattern_eid_roundtrip() {
    let corpus = ["ipn:!.7", "ipn:1.2.3", "dtn://node/svc", "dtn:none"];
    for s in corpus {
        let eid: Eid = s
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse EID {s}: {e}"));
        let pattern = EidPattern::from(eid.clone());
        let back = Eid::try_from(pattern)
            .unwrap_or_else(|e| panic!("{s} did not convert back to an Eid: {e}"));
        assert_eq!(back, eid, "{s} did not round-trip");
    }

    // Exact patterns convert to the canonical Eid forms.
    assert_eq!(
        Eid::try_from("ipn:0.0.0".parse::<EidPattern>().unwrap()).unwrap(),
        Eid::Null
    );
    assert_eq!(
        Eid::try_from("ipn:!.5".parse::<EidPattern>().unwrap()).unwrap(),
        Eid::LocalNode(5)
    );

    // Wildcard patterns are not exact.
    assert!(matches!(
        Eid::try_from("ipn:*.*".parse::<EidPattern>().unwrap()),
        Err(Error::NotExact)
    ));
    // ipn:0.0.s (s != 0) denotes no valid EID.
    assert!(matches!(
        Eid::try_from("ipn:0.0.5".parse::<EidPattern>().unwrap()),
        Err(Error::NotExact)
    ));
}

#[cfg(feature = "dtn-pat-item")]
#[test]
fn dtn_exact_match() {
    assert!(pattern_matches("dtn://node/service", "dtn://node/service"));
    assert!(!pattern_matches("dtn://node/service", "dtn://node/other"));
    assert!(!pattern_matches(
        "dtn://node/service",
        "dtn://other/service"
    ));
}

#[cfg(feature = "dtn-pat-item")]
#[test]
fn dtn_none_pattern() {
    assert!(pattern_matches("dtn:none", "dtn:none"));
    assert!(!pattern_matches("dtn:none", "dtn://node/service"));
}

#[cfg(feature = "dtn-pat-item")]
#[test]
fn dtn_scheme_wildcard() {
    assert!(pattern_matches("dtn:**", "dtn://anything/here"));
}

#[cfg(feature = "dtn-pat-item")]
#[test]
fn dtn_glob_match() {
    // Glob-based DTN matching (single-slash separator, R-13).
    // Single-star matches one demux segment but not across a separator.
    assert!(pattern_matches("dtn://node/*", "dtn://node/service"));
    assert!(!pattern_matches("dtn://node/*", "dtn://other/service"));
    assert!(!pattern_matches("dtn://node/*", "dtn://node/pre/post"));
    // Authority-position single-star.
    assert!(pattern_matches("dtn://*/service", "dtn://node/service"));
    assert!(!pattern_matches("dtn://*/service", "dtn://node/other"));
    // Double-star spans separators.
    assert!(pattern_matches("dtn://node/**", "dtn://node/pre/post"));
    assert!(pattern_matches(
        "dtn://**/some/serv",
        "dtn://node/some/serv"
    ));
}

// Glob matching of the authority is case-insensitive (node names are
// hostname-like). Demux case handling is deliberately left unpinned here: the
// current glob matcher is also case-insensitive for the demux, but RFC 9171
// demux strings are case-sensitive, so that behaviour may yet change.
#[cfg(feature = "dtn-pat-item")]
#[test]
fn dtn_glob_authority_case_insensitive() {
    assert!(pattern_matches(
        "dtn://Node.Example.ORG/*",
        "dtn://node.example.org/svc"
    ));
}

#[test]
fn specificity_score() {
    // IPN fully specific: allocator(32) + node(32) + service(32) = 96, exact → 256 + 96
    assert_eq!(score("ipn:100.1.5"), Some(352));

    // IPN service wildcard: allocator(32) + node(32) + service(0) = 64, not exact
    assert_eq!(score("ipn:100.1.*"), Some(64));

    // IPN node+service wildcard: allocator(32) + node(0) + service(0) = 32
    assert_eq!(score("ipn:100.*.*"), Some(32));

    // IPN range node: allocator(32) + node [10-13] 4 values (30) + service(0) = 62
    assert_eq!(score("ipn:100.[10-13].*"), Some(62));

    // Range counts either side of the power of two: [10-11] is 2 values
    // (31 bits), [10-14] is 5 values (29 bits).
    assert_eq!(score("ipn:100.[10-11].*"), Some(63));
    assert_eq!(score("ipn:100.[10-14].*"), Some(61));

    // Full-width open range: [0+] covers all 2^32 values, 0 literal bits.
    assert_eq!(score("ipn:100.[0+].*"), Some(32));

    // IPN all wildcard → 0
    assert_eq!(score("ipn:*.*.*"), Some(0));

    // ipn:** (ANY pattern item) → 0
    assert_eq!(score("ipn:**"), Some(0));

    // *:** (EidPattern::Any) → 0
    assert_eq!(
        "*:**".parse::<EidPattern>().unwrap().specificity_score(),
        Some(0)
    );

    #[cfg(feature = "dtn-pat-item")]
    {
        // DTN exact: authority(18) + service(3) = 21 literal chars, exact → 256 + 21
        assert_eq!(score("dtn://rover1.example.org/svc"), Some(277));

        // DTN glob: 18 literal chars across full pattern (authority + separator)
        assert_eq!(score("dtn://rover*.example.org/**"), Some(18));

        // DTN none → exact, 0 literal → 256
        assert_eq!(score("dtn:none"), Some(256));

        // DTN any → 0
        assert_eq!(score("dtn:**"), Some(0));
    }

    // Invalid: wildcard allocator with non-wildcard node
    assert_eq!(score("ipn:*.1.*"), None);

    // Invalid: range allocator with specific node
    assert_eq!(score("ipn:[100-200].1.*"), None);

    // Invalid: wildcard node with specific service
    assert_eq!(score("ipn:100.*.5"), None);

    // Invalid: range node with specific service
    assert_eq!(score("ipn:100.[10-13].5"), None);

    // Union set scores as its broadest (min-scoring) member (R-21), not None.
    let a = score("ipn:100.1.*");
    let b = score("ipn:200.1.*");
    assert_eq!(
        "ipn:100.1.*|ipn:200.1.*"
            .parse::<EidPattern>()
            .unwrap()
            .specificity_score(),
        a.min(b),
    );

    // Members with unequal scores: the broadest (min) wins, not the narrowest.
    assert_eq!(score("ipn:100.1.5|ipn:200.1.*"), Some(64));

    // An unscoreable member poisons the whole set.
    assert_eq!(score("ipn:100.1.5|ipn:*.1.*"), None);
}

#[test]
fn specificity_ordering() {
    let exact: EidPattern = "ipn:100.1.5".parse().unwrap(); // score 352
    let svc_wild: EidPattern = "ipn:100.1.*".parse().unwrap(); // score 64
    let node_wild: EidPattern = "ipn:100.*.*".parse().unwrap(); // score 32
    let any: EidPattern = "*:**".parse().unwrap(); // score 0

    // Higher score = Less (comes first)
    assert!(exact < svc_wild);
    assert!(svc_wild < node_wild);
    assert!(node_wild < any);

    // BTreeSet iteration = most specific first
    let mut set = BTreeSet::new();
    set.insert(any.clone());
    set.insert(node_wild.clone());
    set.insert(exact.clone());
    set.insert(svc_wild.clone());

    let ordered: Vec<_> = set.into_iter().collect();
    assert_eq!(ordered, vec![exact, svc_wild, node_wild, any]);
}

// Ord must break ties between equal-score patterns structurally, and stay
// consistent with Eq: otherwise a BTreeMap-backed RIB silently merges two
// equal-specificity routes and drops one.
#[test]
fn equal_score_patterns_are_distinct_in_btreeset() {
    let patterns: Vec<EidPattern> = [
        "ipn:0.1.*", // score 64
        "ipn:0.2.*", // score 64: ties with the previous
        "*:**",      // score 0
        "ipn:*.1.*", // unscoreable (None), treated as 0: ties with *:**
    ]
    .iter()
    .map(|s| s.parse().unwrap())
    .collect();

    let set: BTreeSet<EidPattern> = patterns.iter().cloned().collect();
    assert_eq!(set.len(), patterns.len(), "equal-score patterns collapsed");

    // cmp is consistent with Eq: reflexive equality and antisymmetry.
    for a in &patterns {
        assert_eq!(a.cmp(a), Ordering::Equal);
        for b in &patterns {
            assert_eq!(a.cmp(b), b.cmp(a).reverse());
        }
    }
}

#[test]
fn subset_single_intervals() {
    // Single interval subset checks
    assert!(is_subset("ipn:0.3.4", "ipn:0.3.4")); // exact match
    assert!(is_subset("ipn:0.3.4", "ipn:0.3.*")); // single value subset of wildcard
    assert!(is_subset("ipn:0.3.4", "ipn:0.3.[0-10]")); // single value subset of range
    assert!(is_subset("ipn:0.3.[5-7]", "ipn:0.3.[0-10]")); // range subset of larger range

    assert!(!is_subset("ipn:0.3.*", "ipn:0.3.4")); // wildcard not subset of single
    assert!(!is_subset("ipn:0.3.[0-10]", "ipn:0.3.[5-7]")); // larger range not subset of smaller
    assert!(!is_subset("ipn:0.3.4", "ipn:0.4.4")); // different node
}

#[test]
fn subset_multiple_intervals_in_lhs() {
    // Every lhs interval must be covered by some rhs interval. This rhs merges
    // to [1-10] at parse, so a single rhs interval covers both lhs intervals.
    assert!(is_subset("ipn:0.3.[1-3,7-9]", "ipn:0.3.[1-5,6-10]"));

    // A genuinely multi-interval rhs (gap at 5, no merge): each lhs interval
    // must find its own covering rhs interval.
    assert!(is_subset("ipn:0.3.[1-3,7-9]", "ipn:0.3.[0-4,6-10]"));

    // Another case: lhs=[1-3, 7-9], rhs=[0-10] (single interval covers both)
    assert!(is_subset("ipn:0.3.[1-3,7-9]", "ipn:0.3.[0-10]"));

    // Case where one lhs interval is not covered
    // lhs=[1-3, 15-20], rhs=[1-5, 6-10] => 15-20 not covered => false
    assert!(!is_subset("ipn:0.3.[1-3,15-20]", "ipn:0.3.[1-5,6-10]"));
}

#[test]
fn subset_multiple_intervals_in_rhs() {
    // Single lhs interval covered by one of multiple rhs intervals
    // Note: [1-5,6-10] merges to [1-10] due to adjacency, so use [1-4,6-10] for gap
    assert!(is_subset("ipn:0.3.[7-9]", "ipn:0.3.[1-4,6-10]")); // 7-9 subset of 6-10

    // Single lhs interval NOT covered by any single rhs interval
    // lhs=[1-7], rhs=[1-4, 6-10] (gap at 5) => 1-7 spans across both, not subset of either
    assert!(!is_subset("ipn:0.3.[1-7]", "ipn:0.3.[1-4,6-10]"));

    // Adjacent intervals merge: [1-5,6-10] becomes [1-10], so [1-7] IS a subset
    assert!(is_subset("ipn:0.3.[1-7]", "ipn:0.3.[1-5,6-10]"));
}

#[test]
fn subset_wildcard() {
    // Wildcard is superset of everything
    assert!(is_subset("ipn:0.3.4", "ipn:0.3.*"));
    assert!(is_subset("ipn:0.3.[1-100]", "ipn:0.3.*"));
    assert!(is_subset("ipn:0.3.*", "ipn:0.3.*"));

    // Wildcard is not subset of non-wildcard
    assert!(!is_subset("ipn:0.3.*", "ipn:0.3.[1-100]"));
}

#[test]
fn subset_eid_pattern_set() {
    // Multiple pattern items in the set
    // lhs has two items, both must be subsets of some item in rhs
    assert!(is_subset("ipn:0.3.4|ipn:0.5.6", "ipn:0.*.*"));

    // Any pattern is superset of everything
    assert!(is_subset("ipn:0.3.4", "*:**"));
    assert!(is_subset("ipn:0.3.4|ipn:0.5.6", "*:**"));

    // Any pattern is not subset of non-Any (unless rhs also covers all)
    assert!(!is_subset("*:**", "ipn:0.*.*"));
}

#[test]
fn subset_scheme_wildcards() {
    assert!(is_subset("88:**", "88:**"));
    assert!(!is_subset("88:**", "99:**"));
    assert!(is_subset("foo:**", "foo:**"));
    assert!(!is_subset("88:**", "foo:**"));
    assert!(is_subset("ipn:1.2.3", "ipn:**"));
    // "2:**" parses to the ipn ANY item.
    assert!(is_subset("ipn:1.2.3", "2:**"));
    // "1:**" parses to the dtn Any item.
    #[cfg(feature = "dtn-pat-item")]
    assert!(is_subset("dtn://a/b", "1:**"));
}

#[cfg(feature = "dtn-pat-item")]
#[test]
fn dtn_subset() {
    // Exact vs glob, and the scheme wildcard as superset.
    assert!(is_subset("dtn://node/svc", "dtn://node/*"));
    assert!(is_subset("dtn://node/svc", "dtn:**"));

    // dtn:** does not match the null endpoint, so dtn:none is not a subset.
    assert!(!is_subset("dtn:none", "dtn:**"));
    assert!(is_subset("dtn:none", "dtn:none"));

    // Any vs glob: only the bare `**` glob covers everything Any matches.
    assert!(is_subset("dtn:**", "dtn:**"));
    assert!(!is_subset("dtn:**", "dtn://*/**"));

    // Glob vs glob is currently always true (known unsound TODO in
    // dtn_pattern.rs), so only the correct-direction case is pinned here.
    assert!(is_subset("dtn://node/*", "dtn://node/**"));
}

// ipn:0.0.0 and dtn:none both match exactly the null endpoint, so each is a
// subset of the other across schemes.
#[cfg(feature = "dtn-pat-item")]
#[test]
fn subset_null_endpoint_cross_scheme() {
    assert!(is_subset("ipn:0.0.0", "dtn:none"));
    assert!(is_subset("dtn:none", "ipn:0.0.0"));
    assert!(!is_subset("ipn:0.0.0", "dtn://node/svc"));
    assert!(!is_subset("dtn:none", "ipn:1.2.3"));
}

#[test]
fn expand_local_node_exact() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };

    let pattern: EidPattern = "ipn:!.42".parse().unwrap();
    let expanded = pattern.expand_local_node(&node_id);
    assert!(expanded.is_some());
    assert_eq!(expanded.unwrap().to_string(), "ipn:1.42");
}

#[test]
fn expand_local_node_wildcard_service() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };

    let pattern: EidPattern = "ipn:!.*".parse().unwrap();
    let expanded = pattern.expand_local_node(&node_id);
    assert!(expanded.is_some());
    assert_eq!(expanded.unwrap().to_string(), "ipn:1.*");
}

#[test]
fn expand_local_node_non_local() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };

    let pattern: EidPattern = "ipn:0.2.42".parse().unwrap();
    assert!(pattern.expand_local_node(&node_id).is_none());
}

#[test]
fn expand_local_node_any() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };

    let pattern = EidPattern::Any;
    assert!(pattern.expand_local_node(&node_id).is_none());
}

#[test]
fn expand_local_node_nonzero_allocator() {
    let node_id = IpnNodeId {
        allocator_id: 5,
        node_number: 10,
    };

    let pattern: EidPattern = "ipn:!.42".parse().unwrap();
    let expanded = pattern.expand_local_node(&node_id);
    assert!(expanded.is_some());
    assert_eq!(expanded.unwrap().to_string(), "ipn:5.10.42");
}

// A set mixing the sentinel with ordinary items must keep the unchanged items
// when the sentinel is rewritten.
#[test]
fn expand_local_node_mixed_set() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };

    let pattern: EidPattern = "ipn:!.42|ipn:0.9.9".parse().unwrap();
    let expanded = pattern
        .expand_local_node(&node_id)
        .expect("sentinel should expand");
    assert_eq!(expanded.to_string(), "ipn:1.42|ipn:9.9");
}
