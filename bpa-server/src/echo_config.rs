#![cfg(feature = "echo")]

use super::*;
use hardy_bpv7::eid::Service;
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};

/// Initialize and register echo services based on configuration
pub async fn init(config: &EchoConfig, bpa: &hardy_bpa::bpa::Bpa) {
    if let EchoConfig::Enabled(services) = config {
        info!(
            "Registering {} echo service(s): {:?}",
            services.len(),
            services
        );
        let echo = Arc::new(hardy_echo_service::EchoService::new());
        match echo.register(bpa, services).await {
            Ok(eids) => {
                for eid in eids {
                    info!("Echo service registered at {eid}");
                }
            }
            Err(e) => error!("Failed to register echo service: {e}"),
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

/// Intermediate representation for TOML-friendly deserialization.
/// TOML doesn't handle `deserialize_any` well, so we use an untagged enum.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EchoConfigRaw {
    /// Disabled via false
    Disabled(bool),
    /// Single IPN service number
    SingleNumber(i64),
    /// Single DTN service name (or "off" to disable)
    SingleString(String),
    /// Array of services
    Array(Vec<ServiceElementRaw>),
}

/// Helper for array elements
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ServiceElementRaw {
    Number(i64),
    String(String),
}

impl<'de> Deserialize<'de> for EchoConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let raw = EchoConfigRaw::deserialize(deserializer)?;

        match raw {
            EchoConfigRaw::Disabled(v) => {
                if v {
                    Ok(EchoConfig::default())
                } else {
                    Ok(EchoConfig::Disabled)
                }
            }
            EchoConfigRaw::SingleNumber(n) => {
                if n < 0 {
                    Err(D::Error::custom("service number must be non-negative"))
                } else {
                    Ok(EchoConfig::Enabled(vec![Service::Ipn(n as u32)]))
                }
            }
            EchoConfigRaw::SingleString(s) => {
                if s.eq_ignore_ascii_case("off") {
                    Ok(EchoConfig::Disabled)
                } else if let Ok(n) = s.parse::<u32>() {
                    Ok(EchoConfig::Enabled(vec![Service::Ipn(n)]))
                } else {
                    Ok(EchoConfig::Enabled(vec![Service::Dtn(s.into())]))
                }
            }
            EchoConfigRaw::Array(arr) => {
                if arr.is_empty() {
                    return Ok(EchoConfig::Disabled);
                }
                let mut services = Vec::with_capacity(arr.len());
                for elem in arr {
                    match elem {
                        ServiceElementRaw::Number(n) => {
                            if n < 0 {
                                return Err(D::Error::custom(
                                    "service number must be non-negative",
                                ));
                            }
                            services.push(Service::Ipn(n as u32));
                        }
                        ServiceElementRaw::String(s) => {
                            if let Ok(n) = s.parse::<u32>() {
                                services.push(Service::Ipn(n));
                            } else {
                                services.push(Service::Dtn(s.into()));
                            }
                        }
                    }
                }
                Ok(EchoConfig::Enabled(services))
            }
        }
    }
}
