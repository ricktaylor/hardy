pub mod cla {
    tonic::include_proto!("cla");

    impl From<ClaAddressType> for hardy_bpa::cla::ClaAddressType {
        fn from(value: ClaAddressType) -> Self {
            match value {
                ClaAddressType::Unknown => Self::Unknown(0),
                ClaAddressType::TcpClV4 => Self::TcpClv4,
            }
        }
    }

    impl TryFrom<hardy_bpa::cla::ClaAddressType> for ClaAddressType {
        type Error = tonic::Status;

        fn try_from(value: hardy_bpa::cla::ClaAddressType) -> Result<Self, Self::Error> {
            match value {
                hardy_bpa::cla::ClaAddressType::TcpClv4 => Ok(ClaAddressType::TcpClV4),
                hardy_bpa::cla::ClaAddressType::Unknown(0) => Ok(ClaAddressType::Unknown),
                hardy_bpa::cla::ClaAddressType::Unknown(s) => Err(tonic::Status::invalid_argument(
                    format!("Unknown cla address type {s}"),
                )),
            }
        }
    }

    impl TryFrom<hardy_bpa::cla::ClaAddress> for ClaAddress {
        type Error = tonic::Status;

        fn try_from(value: hardy_bpa::cla::ClaAddress) -> Result<Self, Self::Error> {
            let (address_type, address): (hardy_bpa::cla::ClaAddressType, hardy_bpa::Bytes) =
                value.into();

            Ok(Self {
                address_type: match address_type {
                    hardy_bpa::cla::ClaAddressType::TcpClv4 => ClaAddressType::TcpClV4.into(),
                    hardy_bpa::cla::ClaAddressType::Unknown(0) => ClaAddressType::Unknown.into(),
                    hardy_bpa::cla::ClaAddressType::Unknown(s) => s as i32,
                },
                address,
            })
        }
    }

    impl TryFrom<ClaAddress> for hardy_bpa::cla::ClaAddress {
        type Error = hardy_bpa::cla::Error;

        fn try_from(value: ClaAddress) -> hardy_bpa::cla::Result<Self> {
            (
                match value.address_type.try_into() {
                    Ok(ClaAddressType::Unknown) => hardy_bpa::cla::ClaAddressType::Unknown(0),
                    Ok(ClaAddressType::TcpClV4) => hardy_bpa::cla::ClaAddressType::TcpClv4,
                    Err(_) => hardy_bpa::cla::ClaAddressType::Unknown(value.address_type as u32),
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
                forward_bundle_response::Result::TooBig(max_bundle_size) => {
                    hardy_bpa::cla::ForwardBundleResult::TooBig(max_bundle_size)
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
