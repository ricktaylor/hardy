// Because Prost is too lose with Rustdoc comments
#![allow(clippy::doc_lazy_continuation)]

pub mod proxy;

pub fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (t.unix_timestamp_nanos() / 1_000_000_000) as i64,
        nanos: (t.unix_timestamp_nanos() % 1_000_000_000) as i32,
    }
}

pub fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, tonic::Status> {
    Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
        .map_err(|e| tonic::Status::from_error(e.into()))?
        + time::Duration::nanoseconds(t.nanos.into()))
}

pub mod cla {
    tonic::include_proto!("cla");

    impl TryFrom<ClaAddress> for hardy_bpa::cla::ClaAddress {
        type Error = tonic::Status;

        fn try_from(value: ClaAddress) -> Result<Self, Self::Error> {
            match (value.address_type.try_into(), value.address) {
                (Ok(ClaAddressType::Tcp), address) => {
                    let address = str::from_utf8(&address).map_err(|e| {
                        tonic::Status::invalid_argument(format!("Invalid address: {e}"))
                    })?;
                    let address = address.parse().map_err(|e| {
                        tonic::Status::invalid_argument(format!("Invalid address: {e}"))
                    })?;
                    Ok(hardy_bpa::cla::ClaAddress::Tcp(address))
                }
                (Ok(ClaAddressType::Private) | Err(_), address) => {
                    Ok(hardy_bpa::cla::ClaAddress::Private(address))
                }
            }
        }
    }

    impl From<hardy_bpa::cla::ClaAddress> for ClaAddress {
        fn from(value: hardy_bpa::cla::ClaAddress) -> Self {
            match value {
                hardy_bpa::cla::ClaAddress::Tcp(address) => ClaAddress {
                    address_type: ClaAddressType::Tcp.into(),
                    address: address.to_string().into(),
                },
                hardy_bpa::cla::ClaAddress::Private(address) => ClaAddress {
                    address_type: ClaAddressType::Private.into(),
                    address,
                },
            }
        }
    }

    impl crate::proxy::RecvMsg for BpaToCla {
        type Msg = bpa_to_cla::Msg;

        fn msg_id(&self) -> u32 {
            self.msg_id
        }

        fn msg(self) -> Result<Self::Msg, tonic::Status> {
            match self.msg {
                None => Err(tonic::Status::invalid_argument("Unknown message")),
                Some(Self::Msg::Status(status)) => Err(status.into()),
                Some(msg) => Ok(msg),
            }
        }
    }

    impl crate::proxy::RecvMsg for ClaToBpa {
        type Msg = cla_to_bpa::Msg;

        fn msg_id(&self) -> u32 {
            self.msg_id
        }

        fn msg(self) -> Result<Self::Msg, tonic::Status> {
            match self.msg {
                None => Err(tonic::Status::invalid_argument("Unknown message")),
                Some(Self::Msg::Status(status)) => Err(status.into()),
                Some(msg) => Ok(msg),
            }
        }
    }

    impl crate::proxy::SendMsg for ClaToBpa {
        type Msg = cla_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl crate::proxy::SendMsg for BpaToCla {
        type Msg = bpa_to_cla::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }
}

pub mod service {
    tonic::include_proto!("service");

    impl crate::proxy::RecvMsg for BpaToApp {
        type Msg = bpa_to_app::Msg;

        fn msg_id(&self) -> u32 {
            self.msg_id
        }

        fn msg(self) -> Result<Self::Msg, tonic::Status> {
            match self.msg {
                None => Err(tonic::Status::invalid_argument("Unknown message")),
                Some(Self::Msg::Status(status)) => Err(status.into()),
                Some(msg) => Ok(msg),
            }
        }
    }

    impl crate::proxy::RecvMsg for AppToBpa {
        type Msg = app_to_bpa::Msg;

        fn msg_id(&self) -> u32 {
            self.msg_id
        }

        fn msg(self) -> Result<Self::Msg, tonic::Status> {
            match self.msg {
                None => Err(tonic::Status::invalid_argument("Unknown message")),
                Some(Self::Msg::Status(status)) => Err(status.into()),
                Some(msg) => Ok(msg),
            }
        }
    }

    impl crate::proxy::SendMsg for AppToBpa {
        type Msg = app_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl crate::proxy::SendMsg for BpaToApp {
        type Msg = bpa_to_app::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    // Low-level Service message impls
    impl crate::proxy::RecvMsg for BpaToService {
        type Msg = bpa_to_service::Msg;

        fn msg_id(&self) -> u32 {
            self.msg_id
        }

        fn msg(self) -> Result<Self::Msg, tonic::Status> {
            match self.msg {
                None => Err(tonic::Status::invalid_argument("Unknown message")),
                Some(Self::Msg::Status(status)) => Err(status.into()),
                Some(msg) => Ok(msg),
            }
        }
    }

    impl crate::proxy::RecvMsg for ServiceToBpa {
        type Msg = service_to_bpa::Msg;

        fn msg_id(&self) -> u32 {
            self.msg_id
        }

        fn msg(self) -> Result<Self::Msg, tonic::Status> {
            match self.msg {
                None => Err(tonic::Status::invalid_argument("Unknown message")),
                Some(Self::Msg::Status(status)) => Err(status.into()),
                Some(msg) => Ok(msg),
            }
        }
    }

    impl crate::proxy::SendMsg for ServiceToBpa {
        type Msg = service_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl crate::proxy::SendMsg for BpaToService {
        type Msg = bpa_to_service::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }
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

        impl From<Status> for tonic::Status {
            fn from(value: Status) -> Self {
                Self::new(value.code.into(), value.message)
            }
        }
    }
}
