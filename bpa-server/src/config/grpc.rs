use core::num::NonZeroUsize;
use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};

fn default_drain_timeout() -> Duration {
    Duration::from_secs(5)
}

// A duration written as a humantime string (e.g. `5s`, `1m 30s`);
// zero is allowed, meaning the wait is disabled outright.
mod human_duration {
    use std::time::Duration;

    use serde::Deserialize;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        humantime::parse_duration(&text)
            .map_err(|e| serde::de::Error::custom(format_args!("invalid duration: {e}")))
    }

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&humantime::format_duration(*duration))
    }

    pub mod option {
        use std::time::Duration;

        use serde::Deserialize;

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let Some(text) = Option::<String>::deserialize(deserializer)? else {
                return Ok(None);
            };
            humantime::parse_duration(&text)
                .map(Some)
                .map_err(|e| serde::de::Error::custom(format_args!("invalid duration: {e}")))
        }

        pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            match duration {
                Some(duration) => serializer.collect_str(&humantime::format_duration(*duration)),
                None => serializer.serialize_none(),
            }
        }
    }

    // An optional humantime duration that must not be zero.
    //
    // Every field using this is a stall bound, where zero disables the
    // protection rather than relaxing it: a zero handshake refuses every
    // registration, a zero idle or claim closes every session at its first
    // quiet moment, and a zero grace puts the rate deadline in the past
    // before a byte has moved. Omit the key to take the default.
    pub mod nonzero_option {
        use std::time::Duration;

        pub use super::option::serialize;

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let duration = super::option::deserialize(deserializer)?;
            if duration == Some(Duration::ZERO) {
                return Err(serde::de::Error::custom("duration must not be zero"));
            }
            Ok(duration)
        }
    }
}

// A BPA registration surface the gRPC front end can host. Listed by
// name in `grpc.services`; an unknown name is refused at parse with the
// known ones listed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GrpcService {
    // Remote convergence-layer adapters.
    Cla,
    // Remote low-level services, which exchange whole BPv7 bundles.
    Service,
    // Remote applications, which exchange payloads (ADUs).
    Application,
    // Remote routing agents.
    Routing,
}

// The service list must name at least one surface and each at most once:
// an empty list is a misconfiguration (omit the `grpc` section to run no
// gRPC server), and a repeated surface is a mistake, not a doubled mount.
// Each name re-enters the derived `GrpcService` deserializer, the one
// source of the kebab-case mapping, so an unknown name is refused with
// the valid ones listed.
fn at_least_one_service<'de, D>(deserializer: D) -> Result<Vec<GrpcService>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, IntoDeserializer};

    let names = Vec::<String>::deserialize(deserializer)?;
    if names.is_empty() {
        return Err(D::Error::custom(
            "grpc.services must list at least one service",
        ));
    }
    let mut services = Vec::with_capacity(names.len());
    for name in names {
        let service = GrpcService::deserialize(name.as_str().into_deserializer()).map_err(
            |e: serde::de::value::Error| D::Error::custom(format_args!("grpc.services: {e}")),
        )?;
        if services.contains(&service) {
            return Err(D::Error::custom(format!(
                "grpc.services lists {service:?} more than once"
            )));
        }
        services.push(service);
    }
    Ok(services)
}

#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct LimitsConfig {
    // The longest a new call may take to send its first message: the
    // register that opens a session, or the metadata that opens a
    // streaming data-plane call. Absent defers to the surface default
    // (10s).
    #[serde(with = "human_duration::nonzero_option")]
    pub handshake: Option<Duration>,

    // The longest a live session may leave any single wait unanswered:
    // the next inbound chunk, room for the next outbound chunk, the ack
    // after the last chunk, or room on the session stream for the next
    // event. Exceeding it closes the session. Absent defers to the
    // surface default (30s).
    #[serde(with = "human_duration::nonzero_option")]
    pub idle: Option<Duration>,

    // The longest an announced delivery or forwarding may wait for the
    // call that collects it. Absent defers to the surface default (30s).
    #[serde(with = "human_duration::nonzero_option")]
    pub claim: Option<Duration>,

    // How long a transfer runs before `min_rate` starts to apply. Unused
    // when `min_rate` is zero. Absent defers to the surface default
    // (30s).
    #[serde(with = "human_duration::nonzero_option")]
    pub grace: Option<Duration>,

    // The bytes per second a transfer must sustain once its grace is
    // spent. Zero disables the rate floor, leaving only `idle`. Absent
    // defers to the surface default (1024).
    pub min_rate: Option<u64>,

    // The live sessions this surface serves at once; a registration that
    // finds no free slot is refused with `RESOURCE_EXHAUSTED`. Absent
    // defers to the surface default (64).
    pub max_sessions: Option<NonZeroUsize>,
}

// The `grpc` section: the gRPC front end serving BPA registration to
// remote CLAs, services, applications, and routing agents. Absent runs
// no gRPC server. The transport is owned and assembled by this crate
// (`hardy-proto` provides the per-surface services), so the defaults are
// this crate's own.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct GrpcConfig {
    // The listen address; absent defers to the server default
    // (`[::1]:50051`).
    #[serde(default)]
    pub address: Option<SocketAddr>,

    // The services to expose, e.g. `["application", "cla", "service",
    // "routing"]`; required and non-empty.
    #[serde(deserialize_with = "at_least_one_service")]
    pub services: Vec<GrpcService>,

    // How long a graceful shutdown waits for open gRPC connections to
    // drain before abandoning them, as a humantime string; `0s` cuts
    // them immediately. The drain is shutdown's one unbounded wait: a
    // client holding an unread response stream keeps its connection
    // open indefinitely.
    #[serde(default = "default_drain_timeout", with = "human_duration")]
    pub drain_timeout: Duration,

    #[serde(default)]
    pub limits: LimitsConfig,
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;
    use crate::config::{Config, tests::error_chain};

    // A `grpc` section enabling no services is refused at parse: absent
    // and all-off are different spellings, and only an absent section
    // means "no gRPC server".
    #[test]
    #[serial]
    fn grpc_no_services_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("all-off.yaml");
        std::fs::write(&path, "grpc:\n  services: []\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = error_chain(&err);
        assert!(err.contains("at least one"), "{err}");

        let path = dir.path().join("missing.yaml");
        std::fs::write(&path, "grpc:\n  address: \"[::1]:50051\"\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = error_chain(&err);
        assert!(
            err.contains(r#"missing configuration field "grpc.services""#),
            "{err}"
        );
    }

    // Unknown gRPC service names are refused at parse with the known ones
    // listed.
    #[test]
    #[serial]
    fn grpc_unknown_service_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grpc.yaml");
        std::fs::write(&path, "grpc:\n  services: [\"clas\"]\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = error_chain(&err);
        assert!(err.contains("application"), "{err}");
    }

    // The service list parses into the typed surfaces it names.
    #[test]
    #[serial]
    fn grpc_services_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("list.yaml");
        std::fs::write(&path, "grpc:\n  services: [\"application\", \"routing\"]\n").unwrap();

        let config = Config::load(Some(path)).expect("the service list must parse");
        let services = config.grpc.expect("grpc section must be present").services;
        assert_eq!(
            services,
            vec![GrpcService::Application, GrpcService::Routing]
        );
    }

    // The drain timeout is a humantime string: defaulted when absent,
    // zero allowed (cut connections immediately), garbage refused.
    #[test]
    #[serial]
    fn grpc_drain_timeout_parses_as_humantime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        std::fs::write(&path, "grpc:\n  services: [\"application\"]\n").unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        assert_eq!(config.grpc.unwrap().drain_timeout, Duration::from_secs(5));

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  drain-timeout: 0s\n",
        )
        .unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        assert_eq!(config.grpc.unwrap().drain_timeout, Duration::ZERO);

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  drain-timeout: eventually\n",
        )
        .unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("a non-humantime drain-timeout must be refused");
        };
        let err = error_chain(&err);
        assert!(err.contains("invalid duration"), "{err}");
    }

    #[test]
    #[serial]
    fn grpc_limits_parse_as_humantime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        std::fs::write(&path, "grpc:\n  services: [\"application\"]\n").unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        let limits = config.grpc.unwrap().limits;
        assert_eq!(limits.handshake, None);
        assert_eq!(limits.idle, None);
        assert_eq!(limits.claim, None);
        assert_eq!(limits.grace, None);
        assert_eq!(limits.min_rate, None);
        assert_eq!(limits.max_sessions, None);

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  limits:\n    handshake: 11s\n    idle: 1m 12s\n    claim: 13s\n    grace: 14s\n    min-rate: 4096\n    max-sessions: 17\n",
        )
        .unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        let limits = config.grpc.unwrap().limits;
        assert_eq!(limits.handshake, Some(Duration::from_secs(11)));
        assert_eq!(limits.idle, Some(Duration::from_secs(72)));
        assert_eq!(limits.claim, Some(Duration::from_secs(13)));
        assert_eq!(limits.grace, Some(Duration::from_secs(14)));
        assert_eq!(limits.min_rate, Some(4096));
        assert_eq!(limits.max_sessions, NonZeroUsize::new(17));

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  limits:\n    idle: 2m\n",
        )
        .unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        let limits = config.grpc.unwrap().limits;
        assert_eq!(limits.idle, Some(Duration::from_secs(120)));
        assert_eq!(limits.handshake, None);

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  limits:\n    min-rate: 0\n",
        )
        .unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        let limits = config.grpc.unwrap().limits;
        assert_eq!(limits.min_rate, Some(0));

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  limits:\n    idle: forever\n",
        )
        .unwrap();
        let Err(err) = Config::load(Some(path.clone())) else {
            panic!("a non-humantime idle must be refused");
        };
        let err = error_chain(&err);
        assert!(err.contains("invalid duration"), "{err}");

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  limits:\n    max-sessions: 0\n",
        )
        .unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("a zero max-sessions must be refused");
        };
        let err = error_chain(&err);
        assert!(err.contains("expected a nonzero usize"), "{err}");
    }

    #[test]
    #[serial]
    fn a_zero_stall_bound_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        for key in ["handshake", "idle", "claim", "grace"] {
            std::fs::write(
                &path,
                format!("grpc:\n  services: [\"application\"]\n  limits:\n    {key}: 0s\n"),
            )
            .unwrap();
            let Err(err) = Config::load(Some(path.clone())) else {
                panic!("{key} must refuse a zero duration");
            };
            assert!(
                error_chain(&err).contains("duration must not be zero"),
                "{key}: {}",
                error_chain(&err)
            );
        }
    }

    // A repeated surface is a mistake, not a doubled mount.
    #[test]
    #[serial]
    fn grpc_duplicate_service_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.yaml");
        std::fs::write(&path, "grpc:\n  services: [\"cla\", \"cla\"]\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = error_chain(&err);
        assert!(err.contains("more than once"), "{err}");
    }
}
