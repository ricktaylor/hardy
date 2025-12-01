use super::*;
use ipn_pattern::*;

#[cfg(feature = "dtn-pat-item")]
use dtn_pattern::*;

#[test]
fn tests() {
    ipn_parse("ipn:0.3.4", IpnPatternItem::new(0, 3, Some(4)));
    assert!(ipn_match("ipn:0.3.4", "ipn:0.3.4"));
    assert!(!ipn_match("ipn:0.3.4", "ipn:0.4.0"));
    assert!(!ipn_match("ipn:0.3.4", "ipn:0.4.3"));
    assert!(!ipn_match("ipn:0.3.4", "ipn:1.3.4"));

    ipn_parse(
        "ipn:0.3.*",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Wildcard,
        },
    );
    assert!(ipn_match("ipn:0.3.*", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.*", "ipn:0.3.4"));
    assert!(ipn_match("ipn:0.3.*", "ipn:0.3.9999"));
    assert!(!ipn_match("ipn:0.3.*", "ipn:0.4.3"));
    assert!(!ipn_match("ipn:0.3.*", "ipn:1.3.3"));

    ipn_parse(
        "ipn:0.*.4",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Range(vec![IpnInterval::Number(4)]),
        },
    );
    assert!(ipn_match("ipn:0.*.4", "ipn:0.3.4"));
    assert!(ipn_match("ipn:0.*.4", "ipn:0.999.4"));
    assert!(!ipn_match("ipn:0.*.4", "ipn:0.3.3"));
    assert!(!ipn_match("ipn:0.*.4", "ipn:0.3.9999"));
    assert!(!ipn_match("ipn:0.*.4", "ipn:1.3.4"));

    ipn_parse(
        "ipn:0.3.[0-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    assert!(ipn_match("ipn:0.3.[0-19]", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.[0-19]", "ipn:0.3.4"));
    assert!(ipn_match("ipn:0.3.[0-19]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[0-19]", "ipn:0.3.20"));
    assert!(!ipn_match("ipn:0.3.[0-19]", "ipn:0.2.19"));

    ipn_parse(
        "ipn:0.3.[10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(10..=19)]),
        },
    );
    assert!(ipn_match("ipn:0.3.[10-19]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[10-19]", "ipn:0.3.15"));
    assert!(ipn_match("ipn:0.3.[10-19]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[10-19]", "ipn:0.3.9"));
    assert!(!ipn_match("ipn:0.3.[10-19]", "ipn:0.2.10"));
    assert!(!ipn_match("ipn:0.3.[10-19]", "ipn:1.3.10"));

    ipn_parse(
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
    assert!(ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.2"));
    assert!(ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.4"));
    assert!(!ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.5"));
    assert!(!ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.7"));
    assert!(!ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.9"));
    assert!(ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.15"));
    assert!(ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[0-4,10-19]", "ipn:0.3.20"));

    ipn_parse(
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
    assert!(ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.2"));
    assert!(ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.4"));
    assert!(!ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.5"));
    assert!(!ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.7"));
    assert!(!ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.9"));
    assert!(ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.15"));
    assert!(ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[10-19,0-4]", "ipn:0.3.20"));

    ipn_parse(
        "ipn:0.3.[0-9,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    assert!(ipn_match("ipn:0.3.[0-9,10-19]", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.[0-9,10-19]", "ipn:0.3.9"));
    assert!(ipn_match("ipn:0.3.[0-9,10-19]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[0-9,10-19]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[0-9,10-19]", "ipn:0.3.20"));

    ipn_parse(
        "ipn:0.3.[0-15,10-19]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.9"));
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.14"));
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.15"));
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.16"));
    assert!(ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[0-15,10-19]", "ipn:0.3.20"));

    ipn_parse(
        "ipn:0.3.[10-19,0-9]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(0..=19)]),
        },
    );
    assert!(ipn_match("ipn:0.3.[10-19,0-9]", "ipn:0.3.0"));
    assert!(ipn_match("ipn:0.3.[10-19,0-9]", "ipn:0.3.9"));
    assert!(ipn_match("ipn:0.3.[10-19,0-9]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[10-19,0-9]", "ipn:0.3.19"));
    assert!(!ipn_match("ipn:0.3.[10-19,0-9]", "ipn:0.3.20"));

    ipn_parse(
        "ipn:0.3.[10+]",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(3)]),
            service_number: IpnPattern::Range(vec![IpnInterval::Range(10..=u32::MAX)]),
        },
    );
    assert!(!ipn_match("ipn:0.3.[10+]", "ipn:0.3.1"));
    assert!(!ipn_match("ipn:0.3.[10+]", "ipn:0.3.9"));
    assert!(ipn_match("ipn:0.3.[10+]", "ipn:0.3.10"));
    assert!(ipn_match("ipn:0.3.[10+]", "ipn:0.3.11"));
    assert!(ipn_match("ipn:0.3.[10+]", "ipn:0.3.9999"));

    assert_eq!(
        "*:**".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Any
    );

    ipn_parse(
        "ipn:!.*",
        IpnPatternItem {
            allocator_id: IpnPattern::Range(vec![IpnInterval::Number(0)]),
            node_number: IpnPattern::Range(vec![IpnInterval::Number(u32::MAX)]),
            service_number: IpnPattern::Wildcard,
        },
    );
    assert!(!ipn_match("ipn:!.*", "ipn:0.3.1"));
    assert!(ipn_match("ipn:!.*", "ipn:0.4294967295.0"));
    assert!(ipn_match("ipn:!.*", "ipn:0.4294967295.1"));
    assert!(ipn_match("ipn:!.*", "ipn:0.4294967295.999999"));
    assert!(!ipn_match("ipn:!.*", "ipn:1.4294967295.1"));

    ipn_parse("ipn:**", ipn_pattern::ANY);
    ipn_parse("2:**", ipn_pattern::ANY);

    #[cfg(feature = "dtn-pat-item")]
    {
        dtn_parse(
            "dtn://node/service",
            DtnPatternItem::Exact("node".into(), "service".into()),
        );
        dtn_parse("dtn://node/*", DtnPatternItem::new_glob("node/*").unwrap());
        dtn_parse(
            "dtn://node/**",
            DtnPatternItem::new_glob("node/**").unwrap(),
        );
        dtn_parse(
            "dtn://node/pre/**",
            DtnPatternItem::new_glob("node/pre/**").unwrap(),
        );
        dtn_parse(
            "dtn://**/some/serv",
            DtnPatternItem::new_glob("**/some/serv").unwrap(),
        );
        /*dtn_match(
        "dtn://**/
[%5B^a%5D]",
            DtnSsp {
                node_name: DtnNodeNamePattern::MultiWildcard,
                demux: [DtnSinglePattern::PatternMatch(PatternMatch::Regex(
                    HashableRegEx::try_new("[^a]").unwrap(),
                ))]
                .into(),
                last_wild: false,
            },
        );*/

        assert_eq!(
            "dtn:none".parse::<EidPattern>().expect("Failed to parse"),
            EidPattern::Set([EidPatternItem::DtnPatternItem(DtnPatternItem::None)].into())
        );

        assert_eq!(
            "dtn:**".parse::<EidPattern>().expect("Failed to parse"),
            EidPattern::Set([EidPatternItem::DtnPatternItem(DtnPatternItem::Any)].into())
        );
        assert_eq!(
            "1:**".parse::<EidPattern>().expect("Failed to parse"),
            EidPattern::Set([EidPatternItem::DtnPatternItem(DtnPatternItem::Any)].into())
        );

        assert_eq!(
            "dtn://node/service|ipn:0.3.4"
                .parse::<EidPattern>()
                .expect("Failed to parse"),
            EidPattern::Set(
                [
                    EidPatternItem::DtnPatternItem(DtnPatternItem::Exact(
                        "node".into(),
                        "service".into()
                    )),
                    EidPatternItem::IpnPatternItem(IpnPatternItem::new(0, 3, Some(4)))
                ]
                .into()
            )
        );
    }
}

fn ipn_match(pattern: &str, eid: &str) -> bool {
    pattern
        .parse::<EidPattern>()
        .inspect_err(|e| print!("{e}"))
        .expect("Failed to parse pattern")
        .matches(&eid.parse().expect("Failed to parse EID"))
}

fn ipn_parse(s: &str, expected: IpnPatternItem) {
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
fn dtn_parse(s: &str, expected: DtnPatternItem) {
    match s.parse().expect("Failed to parse") {
        EidPattern::Set(v) => {
            if v.len() != 1 {
                panic!("More than 1 pattern item!");
            }

            let EidPatternItem::DtnPatternItem(i) = &v[0] else {
                panic!("Not a dtn pattern item!")
            };

            assert_eq!(i, &expected)
        }
        EidPattern::Any => panic!("Not an dtn pattern item!"),
    }
}
