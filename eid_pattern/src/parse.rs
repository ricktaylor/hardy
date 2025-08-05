use crate::ipn_pattern::IpnPatternItem;

use super::*;
use winnow::{
    ModalResult, Parser,
    ascii::digit0,
    combinator::{alt, separated, terminated},
    stream::AsChar,
    token::{one_of, take_while},
};

impl std::str::FromStr for EidPattern {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_eid_pattern
            .parse(s)
            .map_err(|e| Error::ParseError(e.to_string()))
    }
}

// eid-pattern = any-scheme-item / eid-pattern-set
fn parse_eid_pattern(input: &mut &str) -> ModalResult<EidPattern> {
    alt((parse_any_scheme_item, parse_eid_pattern_set)).parse_next(input)
}

// any-scheme-item = wildcard ":" multi-wildcard
fn parse_any_scheme_item(input: &mut &str) -> ModalResult<EidPattern> {
    ("*:**").map(|_| EidPattern::Any).parse_next(input)
}

// eid-pattern-set = eid-pattern-item *( "|" eid-pattern-item )
fn parse_eid_pattern_set(input: &mut &str) -> ModalResult<EidPattern> {
    separated(1.., parse_eid_pattern_item, "|")
        .map(|v: Vec<EidPatternItem>| EidPattern::Set(v.into()))
        .parse_next(input)
}

// eid-pattern-item = scheme-pat-item / any-ssp-item
fn parse_eid_pattern_item(input: &mut &str) -> ModalResult<EidPatternItem> {
    alt((parse_scheme_pat_item, parse_any_ssp_item)).parse_next(input)
}

/*
; Extension point at scheme-pat-item for future scheme-specific rules
scheme-pat-item = ipn-pat-item / dtn-pat-item
 */
fn parse_scheme_pat_item(input: &mut &str) -> ModalResult<EidPatternItem> {
    alt((
        ipn_pattern::parse_ipn_pat_item,
        #[cfg(feature = "dtn-pat-item")]
        dtn_pattern::parse_dtn_pat_item,
    ))
    .parse_next(input)
}

// any-ssp-item = (scheme / non-zero-decimal) ":" multi-wildcard
fn parse_any_ssp_item(input: &mut &str) -> ModalResult<EidPatternItem> {
    terminated(
        alt((
            parse_scheme,
            parse_non_zero_decimal.map(|v| match v {
                #[cfg(feature = "dtn-pat-item")]
                1 => EidPatternItem::DtnPatternItem(dtn_pattern::DtnPatternItem::All),
                2 => EidPatternItem::IpnPatternItem(IpnPatternItem::new_any()),
                _ => EidPatternItem::AnyNumericScheme(v),
            }),
        )),
        ":**",
    )
    .parse_next(input)
}

// scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )
fn parse_scheme(input: &mut &str) -> ModalResult<EidPatternItem> {
    (
        one_of(AsChar::is_alpha),
        take_while(0.., (AsChar::is_alphanum, '+', '-', '.')),
    )
        .take()
        .map(|v: &str| EidPatternItem::AnyTextScheme(v.into()))
        .parse_next(input)
}

// non-zero-decimal = (%x31-39 *DIGIT)
fn parse_non_zero_decimal<T>(input: &mut &str) -> ModalResult<T>
where
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::error::Error + Send + Sync + 'static,
{
    (one_of('1'..'9'), digit0)
        .take()
        .try_map(|v: &str| v.parse::<T>())
        .parse_next(input)
}
