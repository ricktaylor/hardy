#![cfg(feature = "echo")]

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Echo service configuration
/// - Absent/null: enabled on service number 7 (default)
/// - Number: enabled on that service number
/// - "off" or "false": disabled
#[derive(Debug, Clone)]
pub enum EchoConfig {
    Enabled(u32),
    Disabled,
}

impl Default for EchoConfig {
    fn default() -> Self {
        EchoConfig::Enabled(7)
    }
}

impl Serialize for EchoConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            EchoConfig::Enabled(n) => serializer.serialize_u32(*n),
            EchoConfig::Disabled => serializer.serialize_str("off"),
        }
    }
}

impl<'de> Deserialize<'de> for EchoConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct EchoConfigVisitor;

        impl<'de> Visitor<'de> for EchoConfigVisitor {
            type Value = EchoConfig;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a service number, \"off\", or \"false\"")
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

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v < 0 {
                    Err(de::Error::custom("service number must be non-negative"))
                } else {
                    Ok(EchoConfig::Enabled(v as u32))
                }
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(EchoConfig::Enabled(v as u32))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match v.to_lowercase().as_str() {
                    "off" | "false" | "disabled" | "no" => Ok(EchoConfig::Disabled),
                    "on" | "true" | "enabled" | "yes" => Ok(EchoConfig::default()),
                    _ => v
                        .parse::<u32>()
                        .map(EchoConfig::Enabled)
                        .map_err(|_| de::Error::custom("expected service number or \"off\"")),
                }
            }
        }

        deserializer.deserialize_any(EchoConfigVisitor)
    }
}
