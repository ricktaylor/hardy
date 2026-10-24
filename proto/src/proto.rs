use super::*;

pub mod cla {
    use core::num::NonZeroU32;

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

    // The lanes a registering CLA declares: an absent declaration is
    // serial, and an explicit zero limit is invalid. The BPA's `Lanes` is
    // path-qualified throughout: the generated oneof is also `Lanes`.
    pub fn declared_lanes(
        declared: Option<register_cla_request::Lanes>,
    ) -> Result<hardy_bpa::cla::Lanes, tonic::Status> {
        match declared {
            None | Some(register_cla_request::Lanes::Serial(())) => {
                Ok(hardy_bpa::cla::Lanes::Serial)
            }
            Some(register_cla_request::Lanes::Limited(n)) => NonZeroU32::new(n)
                .map(hardy_bpa::cla::Lanes::Limited)
                .ok_or_else(|| {
                    tonic::Status::invalid_argument(
                        "lanes.limited 0 is invalid; declare serial instead",
                    )
                }),
            Some(register_cla_request::Lanes::Unlimited(())) => {
                Ok(hardy_bpa::cla::Lanes::Unlimited)
            }
        }
    }

    impl From<hardy_bpa::cla::Lanes> for register_cla_request::Lanes {
        fn from(value: hardy_bpa::cla::Lanes) -> Self {
            match value {
                hardy_bpa::cla::Lanes::Serial => Self::Serial(()),
                hardy_bpa::cla::Lanes::Limited(n) => Self::Limited(n.get()),
                hardy_bpa::cla::Lanes::Unlimited => Self::Unlimited(()),
            }
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
        use hardy_bpa::cla::Lanes;

        use super::*;

        // Every declaration crosses the wire intact.
        #[test]
        fn lanes_round_trip_the_wire() {
            for lanes in [
                Lanes::Serial,
                Lanes::Limited(NonZeroU32::new(4).unwrap()),
                Lanes::Unlimited,
            ] {
                let wire = register_cla_request::Lanes::from(lanes);
                assert_eq!(declared_lanes(Some(wire)).unwrap(), lanes);
            }
        }

        // A client that sends no declaration, such as one built before the
        // field existed, registers serial.
        #[test]
        fn absent_lanes_are_serial() {
            assert_eq!(declared_lanes(None).unwrap(), Lanes::Serial);
        }

        // An explicit zero limit is refused, not read as serial or unlimited.
        #[test]
        fn a_zero_lane_limit_is_invalid() {
            let refused = declared_lanes(Some(register_cla_request::Lanes::Limited(0)));
            assert_eq!(refused.unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }
}

pub mod service {
    use core::num::NonZeroU32;

    use super::*;

    tonic::include_proto!("service");

    // The delivery lanes a registering service or application declares: an
    // absent declaration is serial, and an explicit zero limit is invalid.
    // The BPA's `Lanes` is path-qualified throughout: the generated oneof is
    // also `Lanes`.
    pub fn declared_lanes(
        declared: Option<register_request::Lanes>,
    ) -> Result<hardy_bpa::cla::Lanes, tonic::Status> {
        match declared {
            None | Some(register_request::Lanes::Serial(())) => Ok(hardy_bpa::cla::Lanes::Serial),
            Some(register_request::Lanes::Limited(n)) => NonZeroU32::new(n)
                .map(hardy_bpa::cla::Lanes::Limited)
                .ok_or_else(|| {
                    tonic::Status::invalid_argument(
                        "lanes.limited 0 is invalid; declare serial instead",
                    )
                }),
            Some(register_request::Lanes::Unlimited(())) => Ok(hardy_bpa::cla::Lanes::Unlimited),
        }
    }

    // A registration's delivery lanes on the wire: serial, the default,
    // travels as absence, so a serial registration reads the same to a BPA
    // that predates the field.
    pub fn wire_lanes(lanes: hardy_bpa::cla::Lanes) -> Option<register_request::Lanes> {
        match lanes {
            hardy_bpa::cla::Lanes::Serial => None,
            hardy_bpa::cla::Lanes::Limited(n) => Some(register_request::Lanes::Limited(n.get())),
            hardy_bpa::cla::Lanes::Unlimited => Some(register_request::Lanes::Unlimited(())),
        }
    }

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

    #[cfg(test)]
    mod tests {
        use hardy_bpa::cla::Lanes;

        use super::*;

        // Every declaration crosses the wire intact, serial as absence.
        #[test]
        fn delivery_lanes_round_trip_the_wire() {
            assert_eq!(wire_lanes(Lanes::Serial), None);
            for lanes in [
                Lanes::Serial,
                Lanes::Limited(NonZeroU32::new(4).unwrap()),
                Lanes::Unlimited,
            ] {
                assert_eq!(declared_lanes(wire_lanes(lanes)).unwrap(), lanes);
            }
        }

        // An explicit zero limit is refused, not read as serial or unlimited.
        #[test]
        fn a_zero_delivery_lane_limit_is_invalid() {
            let refused = declared_lanes(Some(register_request::Lanes::Limited(0)));
            assert_eq!(refused.unwrap_err().code(), tonic::Code::InvalidArgument);
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
