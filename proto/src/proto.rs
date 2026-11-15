use super::*;

// The generated code nests the message's oneof in a module named after the
// message, which here is also the package's name.
#[allow(clippy::module_inception)]
pub mod lanes {
    use core::num::NonZeroU32;

    tonic::include_proto!("lanes");

    // `Lanes` here is the generated message; the BPA's declaration is
    // `hardy_bpa::lanes::Lanes`, path-qualified for the collision.

    impl From<hardy_bpa::lanes::Lanes> for Lanes {
        fn from(value: hardy_bpa::lanes::Lanes) -> Self {
            let lanes = match value {
                hardy_bpa::lanes::Lanes::Serial => lanes::Kind::Serial(()),
                hardy_bpa::lanes::Lanes::Limited(n) => lanes::Kind::Limited(n.get()),
                hardy_bpa::lanes::Lanes::Unlimited => lanes::Kind::Unlimited(()),
            };
            Self { kind: Some(lanes) }
        }
    }

    // The lanes a registration declares: an omitted declaration (or an
    // empty one) is serial, and an explicit zero limit is invalid.
    pub fn declared(declared: Option<Lanes>) -> Result<hardy_bpa::lanes::Lanes, tonic::Status> {
        match declared.and_then(|declared| declared.kind) {
            None | Some(lanes::Kind::Serial(())) => Ok(hardy_bpa::lanes::Lanes::Serial),
            Some(lanes::Kind::Limited(n)) => NonZeroU32::new(n)
                .map(hardy_bpa::lanes::Lanes::Limited)
                .ok_or_else(|| {
                    tonic::Status::invalid_argument(
                        "lanes.limited 0 is invalid; declare serial instead",
                    )
                }),
            Some(lanes::Kind::Unlimited(())) => Ok(hardy_bpa::lanes::Lanes::Unlimited),
        }
    }

    // A registration's lanes on the wire: serial, the default, travels as
    // an omitted field, so a serial registration reads the same to a BPA
    // that predates the field.
    pub fn on_wire(lanes: hardy_bpa::lanes::Lanes) -> Option<Lanes> {
        (lanes != hardy_bpa::lanes::Lanes::Serial).then(|| lanes.into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Every declaration crosses the wire intact, serial as an omitted
        // field.
        #[test]
        fn lanes_round_trip_the_wire() {
            assert_eq!(on_wire(hardy_bpa::lanes::Lanes::Serial), None);
            for lanes in [
                hardy_bpa::lanes::Lanes::Serial,
                hardy_bpa::lanes::Lanes::Limited(NonZeroU32::new(4).unwrap()),
                hardy_bpa::lanes::Lanes::Unlimited,
            ] {
                assert_eq!(declared(on_wire(lanes)).unwrap(), lanes);
                assert_eq!(declared(Some(lanes.into())).unwrap(), lanes);
            }
        }

        // A client that sends no declaration, such as one built before the
        // field existed, registers serial, as does an empty message.
        #[test]
        fn absent_lanes_are_serial() {
            assert_eq!(declared(None).unwrap(), hardy_bpa::lanes::Lanes::Serial);
            assert_eq!(
                declared(Some(Lanes { kind: None })).unwrap(),
                hardy_bpa::lanes::Lanes::Serial
            );
        }

        // An explicit zero limit is refused, not read as serial or unlimited.
        #[test]
        fn a_zero_lane_limit_is_invalid() {
            let zero = Lanes {
                kind: Some(lanes::Kind::Limited(0)),
            };
            assert_eq!(
                declared(Some(zero)).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
    }
}

pub mod cla {
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
