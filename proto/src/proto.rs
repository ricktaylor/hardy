use super::*;

pub mod cla {
    // Alias the BPA type to distinguish it from the generated wire message.
    use hardy_bpa::cla::PeerLinkInfo as BpaPeerLinkInfo;
    use prost_types::Timestamp;
    use time::OffsetDateTime;
    use tonic::Status;

    use super::*;

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

    // Protobuf timestamps cover years 0001 through 9999, with nonnegative
    // fractional nanoseconds even for instants before the Unix epoch.
    fn validate_contact_end(value: Timestamp) -> Result<Timestamp, Status> {
        if !(-62_135_596_800..=253_402_300_799).contains(&value.seconds)
            || !(0..1_000_000_000).contains(&value.nanos)
        {
            return Err(Status::invalid_argument("Invalid contact_end timestamp"));
        }
        Ok(value)
    }

    impl TryFrom<BpaPeerLinkInfo> for PeerLinkInfo {
        type Error = Status;

        fn try_from(value: BpaPeerLinkInfo) -> Result<Self, Self::Error> {
            let contact_end = value
                .contact_end
                .map(|t| {
                    validate_contact_end(Timestamp {
                        seconds: t.unix_timestamp(),
                        nanos: t.nanosecond() as i32,
                    })
                })
                .transpose()?;
            Ok(Self {
                bandwidth_bps: value.bandwidth_bps,
                mtu: value.mtu,
                contact_end,
            })
        }
    }

    impl TryFrom<PeerLinkInfo> for BpaPeerLinkInfo {
        type Error = Status;

        fn try_from(value: PeerLinkInfo) -> Result<Self, Self::Error> {
            let contact_end = value
                .contact_end
                .map(|t| {
                    let t = validate_contact_end(t)?;
                    OffsetDateTime::from_unix_timestamp_nanos(
                        i128::from(t.seconds) * 1_000_000_000 + i128::from(t.nanos),
                    )
                    .map_err(|_| Status::invalid_argument("Invalid contact_end timestamp"))
                })
                .transpose()?;
            Ok(Self {
                bandwidth_bps: value.bandwidth_bps,
                mtu: value.mtu,
                contact_end,
            })
        }
    }

    impl proxy::RecvMsg for BpaToCla {
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

    impl proxy::RecvMsg for ClaToBpa {
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

    impl proxy::SendMsg for ClaToBpa {
        type Msg = cla_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl proxy::SendMsg for BpaToCla {
        type Msg = bpa_to_cla::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use tonic::Code;

        use super::*;

        #[test]
        fn peer_link_info_round_trip_preserves_fractional_instants() {
            for seconds in [-62_135_596_800, -1, 0, 253_402_300_799] {
                let wire = PeerLinkInfo {
                    bandwidth_bps: Some(u64::MAX),
                    mtu: Some(u32::MAX),
                    contact_end: Some(Timestamp {
                        seconds,
                        nanos: 987_654_321,
                    }),
                };
                let info = BpaPeerLinkInfo::try_from(wire).unwrap();
                assert_eq!(info.contact_end.unwrap().unix_timestamp(), seconds);
                assert_eq!(info.contact_end.unwrap().nanosecond(), 987_654_321);
                assert_eq!(PeerLinkInfo::try_from(info).unwrap(), wire);
            }
        }

        #[test]
        fn peer_link_info_preserves_unknown_and_zero_values() {
            for wire in [
                PeerLinkInfo::default(),
                PeerLinkInfo {
                    bandwidth_bps: Some(0),
                    mtu: Some(0),
                    contact_end: None,
                },
            ] {
                let info = BpaPeerLinkInfo::try_from(wire).unwrap();
                assert_eq!(info.contact_end, None);
                assert_eq!(PeerLinkInfo::try_from(info).unwrap(), wire);
            }
        }

        #[test]
        fn peer_link_info_rejects_invalid_wire_timestamps() {
            for (seconds, nanos) in [
                (0, -1),
                (0, 1_000_000_000),
                (-62_135_596_801, 0),
                (253_402_300_800, 0),
                (i64::MIN, 0),
                (i64::MAX, 0),
            ] {
                let error = BpaPeerLinkInfo::try_from(PeerLinkInfo {
                    contact_end: Some(Timestamp { seconds, nanos }),
                    ..Default::default()
                })
                .unwrap_err();
                assert_eq!(error.code(), Code::InvalidArgument);
            }
        }

        #[test]
        fn peer_link_info_rejects_dates_before_protobuf_range() {
            let info = BpaPeerLinkInfo {
                contact_end: Some(OffsetDateTime::from_unix_timestamp(-62_135_596_801).unwrap()),
                ..Default::default()
            };
            assert_eq!(
                PeerLinkInfo::try_from(info).unwrap_err().code(),
                Code::InvalidArgument
            );
        }
    }
}

pub mod service {
    use super::*;

    tonic::include_proto!("service");

    impl proxy::RecvMsg for BpaToApp {
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

    impl proxy::RecvMsg for AppToBpa {
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

    impl proxy::SendMsg for AppToBpa {
        type Msg = app_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl proxy::SendMsg for BpaToApp {
        type Msg = bpa_to_app::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    // Low-level Service message impls
    impl proxy::RecvMsg for BpaToService {
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

    impl proxy::RecvMsg for ServiceToBpa {
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

    impl proxy::SendMsg for ServiceToBpa {
        type Msg = service_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl proxy::SendMsg for BpaToService {
        type Msg = bpa_to_service::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }
}

pub mod routing {
    use super::*;

    tonic::include_proto!("routing");

    impl proxy::RecvMsg for BpaToAgent {
        type Msg = bpa_to_agent::Msg;

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

    impl proxy::RecvMsg for AgentToBpa {
        type Msg = agent_to_bpa::Msg;

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

    impl proxy::SendMsg for AgentToBpa {
        type Msg = agent_to_bpa::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl proxy::SendMsg for BpaToAgent {
        type Msg = bpa_to_agent::Msg;

        fn compose(msg_id: u32, msg: Self::Msg) -> Self {
            Self {
                msg_id,
                msg: Some(msg),
            }
        }
    }

    impl TryFrom<RouteAction> for hardy_bpa::routing::RouteAction {
        type Error = tonic::Status;

        fn try_from(value: RouteAction) -> Result<Self, Self::Error> {
            match value.action {
                None => Err(tonic::Status::invalid_argument("Missing action")),
                Some(route_action::Action::Drop(drop)) => {
                    let reason = if drop.has_reason {
                        Some(drop.reason_code.try_into().map_err(|e| {
                            tonic::Status::invalid_argument(format!("Invalid reason code: {e}"))
                        })?)
                    } else {
                        None
                    };
                    Ok(hardy_bpa::routing::RouteAction::Drop(reason))
                }
                Some(route_action::Action::Reflect(_)) => {
                    Ok(hardy_bpa::routing::RouteAction::Reflect)
                }
                Some(route_action::Action::Via(eid)) => {
                    let eid = eid.parse().map_err(|e| {
                        tonic::Status::invalid_argument(format!("Invalid EID: {e}"))
                    })?;
                    Ok(hardy_bpa::routing::RouteAction::Via(eid))
                }
            }
        }
    }

    impl From<&hardy_bpa::routing::RouteAction> for RouteAction {
        fn from(value: &hardy_bpa::routing::RouteAction) -> Self {
            match value {
                hardy_bpa::routing::RouteAction::Drop(reason) => RouteAction {
                    action: Some(route_action::Action::Drop(DropAction {
                        has_reason: reason.is_some(),
                        reason_code: reason.map(|r| r.into()).unwrap_or(0),
                    })),
                },
                hardy_bpa::routing::RouteAction::Reflect => RouteAction {
                    action: Some(route_action::Action::Reflect(ReflectAction {})),
                },
                hardy_bpa::routing::RouteAction::Via(eid) => RouteAction {
                    action: Some(route_action::Action::Via(eid.to_string())),
                },
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
