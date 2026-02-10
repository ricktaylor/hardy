#![cfg(feature = "echo")]

use super::*;
use hardy_bpv7::eid::Service;
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};

/// Initialize and register echo services based on configuration
pub async fn init(config: EchoConfig, bpa: &hardy_bpa::bpa::Bpa) {
    if let EchoConfig::Enabled(services) = config {
        let echo = Arc::new(hardy_echo_service::EchoService::new());
        for service in services {
            match bpa
                .register_service(Some(service.clone()), echo.clone())
                .await
            {
                Ok(eid) => info!("Echo service registered at {eid}"),
                Err(e) => error!("Failed to register echo service on {service}: {e}"),
            }
        }
    }
}

/// Echo service configuration
///
/// Supports flexible input formats:
/// - Absent: enabled on IPN service 7 and DTN service "echo" (default)
/// - `false` or `null`: disabled
/// - Number (e.g., `7`): enabled on that IPN service number
/// - String (e.g., `"echo"`): enabled on that DTN service name
/// - `"off"`: disabled (reserved keyword)
/// - Array (e.g., `["echo", 7, 150]`): enabled on multiple services
#[derive(Debug, Clone)]
pub enum EchoConfig {
    Enabled(Vec<Service>),
    Disabled,
}

impl Default for EchoConfig {
    fn default() -> Self {
        EchoConfig::Enabled(vec![Service::Ipn(7), Service::Dtn("echo".into())])
    }
}

impl Serialize for EchoConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            EchoConfig::Enabled(services) if services.len() == 1 => match &services[0] {
                Service::Ipn(n) => serializer.serialize_u32(*n),
                Service::Dtn(s) => serializer.serialize_str(s),
            },
            EchoConfig::Enabled(services) => {
                let mut seq = serializer.serialize_seq(Some(services.len()))?;
                for svc in services {
                    match svc {
                        Service::Ipn(n) => seq.serialize_element(n)?,
                        Service::Dtn(s) => seq.serialize_element(s.as_ref())?,
                    }
                }
                seq.end()
            }
            EchoConfig::Disabled => serializer.serialize_bool(false),
        }
    }
}

/// Parse a single service from a string or number
fn parse_service<E: serde::de::Error>(s: &str) -> Result<Service, E> {
    // Try to parse as number first
    if let Ok(n) = s.parse::<u32>() {
        Ok(Service::Ipn(n))
    } else {
        Ok(Service::Dtn(s.into()))
    }
}

impl<'de> Deserialize<'de> for EchoConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, SeqAccess, Visitor};

        struct EchoConfigVisitor;

        impl<'de> Visitor<'de> for EchoConfigVisitor {
            type Value = EchoConfig;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter
                    .write_str("a service number, service name, array of services, false, or null")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v {
                    Ok(EchoConfig::default())
                } else {
                    Ok(EchoConfig::Disabled)
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(EchoConfig::Disabled)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(EchoConfig::Disabled)
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v < 0 {
                    Err(de::Error::custom("service number must be non-negative"))
                } else {
                    Ok(EchoConfig::Enabled(vec![Service::Ipn(v as u32)]))
                }
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(EchoConfig::Enabled(vec![Service::Ipn(v as u32)]))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                // "off" is reserved as disable keyword
                if v.eq_ignore_ascii_case("off") {
                    return Ok(EchoConfig::Disabled);
                }
                Ok(EchoConfig::Enabled(vec![parse_service(v)?]))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut services = Vec::new();

                while let Some(elem) = seq.next_element::<ServiceElement>()? {
                    services.push(elem.0);
                }

                if services.is_empty() {
                    Ok(EchoConfig::Disabled)
                } else {
                    Ok(EchoConfig::Enabled(services))
                }
            }
        }

        deserializer.deserialize_any(EchoConfigVisitor)
    }
}

/// Helper for deserializing individual array elements
struct ServiceElement(Service);

impl<'de> Deserialize<'de> for ServiceElement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct ServiceElementVisitor;

        impl<'de> Visitor<'de> for ServiceElementVisitor {
            type Value = ServiceElement;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a service number or service name")
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v < 0 {
                    Err(de::Error::custom("service number must be non-negative"))
                } else {
                    Ok(ServiceElement(Service::Ipn(v as u32)))
                }
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ServiceElement(Service::Ipn(v as u32)))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ServiceElement(parse_service(v)?))
            }
        }

        deserializer.deserialize_any(ServiceElementVisitor)
    }
}
