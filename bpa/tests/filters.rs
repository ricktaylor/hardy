//! The stock read filters, driven through the public `filter` API.

mod rfc9171 {
    use hardy_bpa::{
        bundle::Bundle,
        filter::{ReadFilter, ReadResult, rfc9171::Rfc9171ValidityFilter},
    };
    use hardy_bpv7::{
        block::{BibCoverage, Block},
        crc::CrcType,
        creation_timestamp::CreationTimestamp,
        status_report::ReasonCode,
    };

    // A clocked bundle whose primary block (block 0) carries the given
    // integrity state. The Bundle Age check never fires for it.
    fn make_bundle(crc_type: CrcType, bib: BibCoverage) -> Bundle {
        let mut bundle = Bundle {
            bundle: Default::default(),
            metadata: Default::default(),
        };
        bundle.bundle.id.timestamp = CreationTimestamp::now();
        bundle.bundle.crc_type = crc_type;
        bundle.bundle.blocks.insert(
            0,
            Block {
                bib,
                ..Default::default()
            },
        );
        bundle
    }

    // A bundle without a clocked creation timestamp and with no primary
    // block entry, so only the Bundle Age check can fire.
    fn make_unclocked_bundle(age: Option<core::time::Duration>) -> Bundle {
        let mut bundle = Bundle {
            bundle: Default::default(),
            metadata: Default::default(),
        };
        bundle.bundle.id.timestamp = CreationTimestamp::from_parts(None, 1);
        bundle.bundle.age = age;
        bundle
    }

    async fn run(filter: &Rfc9171ValidityFilter, bundle: &Bundle) -> ReadResult {
        filter.filter(bundle, &[]).await.unwrap()
    }

    #[tokio::test]
    async fn no_crc_no_bib_drops_block_unintelligible() {
        let bundle = make_bundle(CrcType::None, BibCoverage::None);
        assert!(matches!(
            run(&Rfc9171ValidityFilter::new(), &bundle).await,
            ReadResult::Drop(Some(ReasonCode::BlockUnintelligible))
        ));
    }

    #[tokio::test]
    async fn crc_alone_passes() {
        let bundle = make_bundle(CrcType::CRC32_CASTAGNOLI, BibCoverage::None);
        assert!(matches!(
            run(&Rfc9171ValidityFilter::new(), &bundle).await,
            ReadResult::Continue
        ));
    }

    #[tokio::test]
    async fn bib_alone_passes() {
        let bundle = make_bundle(CrcType::None, BibCoverage::Some(2));
        assert!(matches!(
            run(&Rfc9171ValidityFilter::new(), &bundle).await,
            ReadResult::Continue
        ));
    }

    #[tokio::test]
    async fn unclocked_without_age_drops_lifetime_expired() {
        let bundle = make_unclocked_bundle(None);
        assert!(matches!(
            run(&Rfc9171ValidityFilter::new(), &bundle).await,
            ReadResult::Drop(Some(ReasonCode::LifetimeExpired))
        ));
    }

    #[tokio::test]
    async fn unclocked_with_age_passes() {
        let bundle = make_unclocked_bundle(Some(core::time::Duration::from_secs(60)));
        assert!(matches!(
            run(&Rfc9171ValidityFilter::new(), &bundle).await,
            ReadResult::Continue
        ));
    }

    #[tokio::test]
    async fn setters_disable_the_checks() {
        let filter = Rfc9171ValidityFilter::new().primary_block_integrity(false);
        let bundle = make_bundle(CrcType::None, BibCoverage::None);
        assert!(matches!(run(&filter, &bundle).await, ReadResult::Continue));

        let filter = Rfc9171ValidityFilter::new().bundle_age_required(false);
        let bundle = make_unclocked_bundle(None);
        assert!(matches!(run(&filter, &bundle).await, ReadResult::Continue));
    }
}

mod validity {
    use core::time::Duration;

    use hardy_bpa::{
        bundle::Bundle,
        filter::{ReadFilter, ReadResult, validity::BundleValidityFilter},
    };
    use hardy_bpv7::{
        creation_timestamp::CreationTimestamp, hop_info::HopInfo, status_report::ReasonCode,
    };

    fn make_bundle(lifetime: Duration, hop_count: Option<HopInfo>) -> Bundle {
        let mut bundle = Bundle {
            bundle: Default::default(),
            metadata: Default::default(),
        };
        bundle.bundle.id.timestamp = CreationTimestamp::now();
        bundle.bundle.lifetime = lifetime;
        bundle.bundle.hop_count = hop_count;
        bundle
    }

    async fn run(bundle: &Bundle) -> ReadResult {
        BundleValidityFilter.filter(bundle, &[]).await.unwrap()
    }

    #[tokio::test]
    async fn fresh_bundle_continues() {
        let bundle = make_bundle(Duration::from_secs(3600), None);
        assert!(matches!(run(&bundle).await, ReadResult::Continue));
    }

    #[tokio::test]
    async fn expired_bundle_drops_with_lifetime_expired() {
        // A zero lifetime expires the bundle at its creation instant.
        let bundle = make_bundle(Duration::ZERO, None);
        assert!(matches!(
            run(&bundle).await,
            ReadResult::Drop(Some(ReasonCode::LifetimeExpired))
        ));
    }

    #[tokio::test]
    async fn hop_count_at_limit_continues() {
        // The check is strictly greater-than: count == limit still passes.
        let bundle = make_bundle(
            Duration::from_secs(3600),
            Some(HopInfo { limit: 5, count: 5 }),
        );
        assert!(matches!(run(&bundle).await, ReadResult::Continue));
    }

    #[tokio::test]
    async fn hop_count_over_limit_drops_with_hop_limit_exceeded() {
        let bundle = make_bundle(
            Duration::from_secs(3600),
            Some(HopInfo { limit: 5, count: 6 }),
        );
        assert!(matches!(
            run(&bundle).await,
            ReadResult::Drop(Some(ReasonCode::HopLimitExceeded))
        ));
    }
}
