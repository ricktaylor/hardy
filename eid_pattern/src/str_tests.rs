use super::*;
use ipn_pattern::*;

#[cfg(feature = "dtn-pat-item")]
use dtn_pattern::*;

#[test]
fn tests() {
    ipn_match("ipn:0.3.4", IpnPatternItem::new(0, 3, 4));
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
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    ipn_match(
        "ipn:0.3.[10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(10..=19)]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-4,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![
                IpnInterval::Range(0..=4),
                IpnInterval::Range(10..=19),
            ]),
        },
    );
    ipn_match(
        "ipn:0.3.[10-19,0-4]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![
                IpnInterval::Range(0..=4),
                IpnInterval::Range(10..=19),
            ]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-9,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    ipn_match(
        "ipn:0.3.[0-15,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    ipn_match(
        "ipn:0.3.[10-19,0-9]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    ipn_match(
        "ipn:0.3.[10+]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(10..=u32::MAX)]),
        },
    );
    assert_eq!(
        "*:**".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Any
    );

    ipn_match(
        "ipn:!.*",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(u32::MAX)]),
            service_number: IpnPattern::Wildcard,
        },
    );

    ipn_match("ipn:**", IpnPatternItem::new_any());
    ipn_match("2:**", IpnPatternItem::new_any());

    #[cfg(feature = "dtn-pat-item")]
    {
        dtn_match(
            "dtn://node/service",
            DtnSsp::new("node".into(), ["service".into()].into(), false),
        );
        dtn_match(
            "dtn://node/*",
            DtnSsp {
                node_name: DtnNodeNamePattern::PatternMatch(PatternMatch::Exact("node".into())),
                demux: [DtnSinglePattern::Wildcard].into(),
                last_wild: false,
            },
        );
        dtn_match("dtn://node/**", DtnSsp::new("node".into(), [].into(), true));
        dtn_match(
            "dtn://node/pre/**",
            DtnSsp::new("node".into(), ["pre".into()].into(), true),
        );
        dtn_match(
            "dtn://**/some/serv",
            DtnSsp {
                node_name: DtnNodeNamePattern::MultiWildcard,
                demux: [
                    DtnSinglePattern::PatternMatch(PatternMatch::Exact("some".into())),
                    DtnSinglePattern::PatternMatch(PatternMatch::Exact("serv".into())),
                ]
                .into(),
                last_wild: false,
            },
        );
        dtn_match(
            "dtn://**/[%5B^a%5D]",
            DtnSsp {
                node_name: DtnNodeNamePattern::MultiWildcard,
                demux: [DtnSinglePattern::PatternMatch(PatternMatch::Regex(
                    HashableRegEx::try_new("[^a]").unwrap(),
                ))]
                .into(),
                last_wild: false,
            },
        );

        assert_eq!(
            "dtn:none".parse::<EidPattern>().expect("Failed to parse"),
            EidPattern::Set([EidPatternItem::DtnPatternItem(DtnPatternItem::DtnNone)].into())
        );

        assert_eq!(
            "dtn:**".parse::<EidPattern>().expect("Failed to parse"),
            EidPattern::Set([EidPatternItem::DtnPatternItem(DtnPatternItem::new_any())].into())
        );
        assert_eq!(
            "1:**".parse::<EidPattern>().expect("Failed to parse"),
            EidPattern::Set([EidPatternItem::DtnPatternItem(DtnPatternItem::new_any())].into())
        );

        assert_eq!(
            "dtn://node/service|ipn:0.3.4"
                .parse::<EidPattern>()
                .expect("Failed to parse"),
            EidPattern::Set(
                [
                    EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(DtnSsp::new(
                        "node".into(),
                        ["service".into()].into(),
                        false
                    ))),
                    EidPatternItem::IpnPatternItem(IpnPatternItem::new(0, 3, 4))
                ]
                .into()
            )
        );
    }
}

fn ipn_match(s: &str, expected: IpnPatternItem) {
    match s
        .parse()
        .inspect_err(|e| print!("{e}"))
        .expect("Failed to parse")
    {
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

#[cfg(feature = "dtn-pat-item")]
fn dtn_match(s: &str, expected: DtnSsp) {
    match s.parse().expect("Failed to parse") {
        EidPattern::Set(v) => {
            if v.len() != 1 {
                panic!("More than 1 pattern item!");
            }

            let EidPatternItem::DtnPatternItem(DtnPatternItem::DtnSsp(i)) = &v[0] else {
                panic!("Not a dtn pattern item!")
            };

            assert_eq!(i, &expected)
        }
        EidPattern::Any => panic!("Not an dtn pattern item!"),
    }
}
