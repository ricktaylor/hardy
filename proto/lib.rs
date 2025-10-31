pub mod cla {
    tonic::include_proto!("cla");

    impl From<ClaAddressType> for hardy_bpa::cla::ClaAddressType {
        fn from(value: ClaAddressType) -> Self {
            match value {
                ClaAddressType::Private => Self::Private,
                ClaAddressType::Tcp => Self::Tcp,
            }
        }
    }

    impl From<hardy_bpa::cla::ClaAddressType> for ClaAddressType {
        fn from(value: hardy_bpa::cla::ClaAddressType) -> Self {
            match value {
                hardy_bpa::cla::ClaAddressType::Tcp => ClaAddressType::Tcp,
                hardy_bpa::cla::ClaAddressType::Private => ClaAddressType::Private,
            }
        }
    }

    impl From<hardy_bpa::cla::ClaAddress> for ClaAddress {
        fn from(value: hardy_bpa::cla::ClaAddress) -> Self {
            let (address_type, address): (hardy_bpa::cla::ClaAddressType, hardy_bpa::Bytes) =
                value.into();

            Self {
                address_type: match address_type {
                    hardy_bpa::cla::ClaAddressType::Tcp => ClaAddressType::Tcp.into(),
                    hardy_bpa::cla::ClaAddressType::Private => ClaAddressType::Private.into(),
                },
                address,
            }
        }
    }

    impl TryFrom<ClaAddress> for hardy_bpa::cla::ClaAddress {
        type Error = hardy_bpa::cla::Error;

        fn try_from(value: ClaAddress) -> hardy_bpa::cla::Result<Self> {
            (
                match value.address_type.try_into() {
                    Ok(ClaAddressType::Private) => hardy_bpa::cla::ClaAddressType::Private,
                    Ok(ClaAddressType::Tcp) => hardy_bpa::cla::ClaAddressType::Tcp,
                    Err(_) => hardy_bpa::cla::ClaAddressType::Private,
                },
                value.address,
            )
                .try_into()
        }
    }

    impl From<forward_bundle_response::Result> for hardy_bpa::cla::ForwardBundleResult {
        fn from(value: forward_bundle_response::Result) -> Self {
            match value {
                forward_bundle_response::Result::Sent(_) => {
                    hardy_bpa::cla::ForwardBundleResult::Sent
                }
                forward_bundle_response::Result::NoNeighbour(_) => {
                    hardy_bpa::cla::ForwardBundleResult::NoNeighbour
                }
            }
        }
    }
}

pub mod application {
    tonic::include_proto!("application");
}

pub mod google {
    pub mod rpc {
        tonic::include_proto!("google.rpc");

        impl From<tonic::Status> for Status {
            fn from(value: tonic::Status) -> Self {
                Self {
                    code: value.code().into(),
                    message: value.message().to_string(),
                    details: Vec::new(),
                }
            }
        }
    }
}
