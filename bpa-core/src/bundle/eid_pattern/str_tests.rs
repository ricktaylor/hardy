use super::*;
use std::ops::RangeInclusive;

#[test]
fn tests() {
    ipn_match(
        "ipn:0.3.4",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Number(4)]),
        },
    );
    ipn_match(
        "ipn:0.3.*",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Wildcard,
        },
    );
    ipn_match(
        "ipn:0.*.4",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Range(vec![IpnInterval::Number(4)]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(RangeInclusive::new(0, 19))]),
        },
    );
    ipn_match(
        "ipn:0.3.[10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(RangeInclusive::new(
                10, 19,
            ))]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-4,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![
                IpnInterval::Range(RangeInclusive::new(0, 4)),
                IpnInterval::Range(RangeInclusive::new(10, 19)),
            ]),
        },
    );
    ipn_match(
        "ipn:0.3.[10-19,0-4]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![
                IpnInterval::Range(RangeInclusive::new(0, 4)),
                IpnInterval::Range(RangeInclusive::new(10, 19)),
            ]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-9,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(RangeInclusive::new(0, 19))]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-15,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(RangeInclusive::new(0, 19))]),
        },
    );
    ipn_match(
        "ipn:0.3.[10-19,0-9]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(RangeInclusive::new(0, 19))]),
        },
    );
    assert_eq!(
        "*:**".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Any
    );

    ipn_match("ipn:**", IpnPatternItem::new_any());
    ipn_match("2:**", IpnPatternItem::new_any());

    dtn_match(
        "dtn://node/service",
        DtnSsp {
            authority: DtnAuthPattern::PatternMatch(PatternMatch::Exact("node".to_string())),
            singles: Vec::new(),
            last: DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Exact(
                "service".to_string(),
            ))),
        },
    );
    dtn_match(
        "dtn://node/*",
        DtnSsp {
            authority: DtnAuthPattern::PatternMatch(PatternMatch::Exact("node".to_string())),
            singles: Vec::new(),
            last: DtnLastPattern::Single(DtnSinglePattern::Wildcard),
        },
    );
    dtn_match(
        "dtn://node/**",
        DtnSsp {
            authority: DtnAuthPattern::PatternMatch(PatternMatch::Exact("node".to_string())),
            singles: Vec::new(),
            last: DtnLastPattern::MultiWildcard,
        },
    );
    dtn_match(
        "dtn://node/pre/**",
        DtnSsp {
            authority: DtnAuthPattern::PatternMatch(PatternMatch::Exact("node".to_string())),
            singles: vec![DtnSinglePattern::PatternMatch(PatternMatch::Exact(
                "pre".to_string(),
            ))],
            last: DtnLastPattern::MultiWildcard,
        },
    );
    dtn_match(
        "dtn://**/some/serv",
        DtnSsp {
            authority: DtnAuthPattern::MultiWildcard,
            singles: vec![DtnSinglePattern::PatternMatch(PatternMatch::Exact(
                "some".to_string(),
            ))],
            last: DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Exact(
                "serv".to_string(),
            ))),
        },
    );
    dtn_match(
        "dtn://**/[^a]",
        DtnSsp {
            authority: DtnAuthPattern::MultiWildcard,
            singles: Vec::new(),
            last: DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Regex(
                regex::Regex::new("^a").unwrap(),
            ))),
        },
    );

    assert_eq!(
        "dtn:none".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Set(vec![EidPatternItem::DtnPatternItem(DtnPatternItem::None)])
    );

    assert_eq!(
        "dtn:**".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Set(vec![EidPatternItem::DtnPatternItem(
            DtnPatternItem::new_any()
        )])
    );
    assert_eq!(
        "1:**".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Set(vec![EidPatternItem::DtnPatternItem(
            DtnPatternItem::new_any()
        )])
    );

    assert_eq!(
        "dtn://node/service|ipn:0.3.4"
            .parse::<EidPattern>()
            .expect("Failed to parse"),
        EidPattern::Set(vec![
            EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(DtnSsp {
                authority: DtnAuthPattern::PatternMatch(PatternMatch::Exact("node".to_string())),
                singles: Vec::new(),
                last: DtnLastPattern::Single(DtnSinglePattern::PatternMatch(PatternMatch::Exact(
                    "service".to_string(),
                ))),
            })),
            EidPatternItem::IpnPatternItem(IpnPatternItem {
                allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
                node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
                service_number: IpnPattern::Range(vec![IpnInterval::Number(4)]),
            })
        ])
    );
}

fn ipn_match(s: &str, expected: IpnPatternItem) {
    match s.parse::<EidPattern>().expect("Failed to parse") {
        EidPattern::Set(v) => {
            if v.len() != 1 {
                panic!("More than 1 pattern item!");
            }

            let EidPatternItem::IpnPatternItem(i) = &v[0] else {
                panic!("Not an ipn pattern item!")
            };

            assert_eq!(i, &expected)
        }
        EidPattern::Any => panic!("Not an ipn pattern item!"),
    }
}

fn dtn_match(s: &str, expected: DtnSsp) {
    match s.parse::<EidPattern>().expect("Failed to parse") {
        EidPattern::Set(v) => {
            if v.len() != 1 {
                panic!("More than 1 pattern item!");
            }

            let EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(i)) = &v[0] else {
                panic!("Not a dtn pattern item!")
            };

            assert_eq!(i, &expected)
        }
        EidPattern::Any => panic!("Not an ipn pattern item!"),
    }
}
