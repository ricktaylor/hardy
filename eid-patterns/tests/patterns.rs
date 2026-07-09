use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::{EidPattern, EidPatternItem};

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
                    s[..s.len() - 3].parse().unwrap()
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
