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
    assert!(matches!(
        "*:**".parse::<EidPattern>().expect("Failed to parse"),
        EidPattern::Any
    ));

    ipn_match("ipn:**", IpnPatternItem::new_any());
    ipn_match("2:**", IpnPatternItem::new_any());
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
