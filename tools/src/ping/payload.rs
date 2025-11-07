use super::*;

// Payload structure for ping service

/* (1 byte)
 * A "service" or "type" flag.
 * 0x01 = PING (This is what bping sends)
 * 0x02 = PONG (This is what the reply service sends back)
 */
//pub service_flag: u8,

/*
 * (4 bytes, unsigned integer)
 * The sequence number of this specific ping packet,
 * starting from 0 or 1.
 */
//pub seqno: u32,

/*
 * The time the ping was sent (seconds part).
 * This is the 'sec' field from a timeval struct.
 */
//pub timeval_sec: i64,

/*
 * The time the ping was sent (microseconds or nanoseconds part).
 * This is the 'usec' or 'nsec' field from a timeval/timespec struct.
 * The bping client and pong server must agree on the unit.
 */
//pub timeval_nsec: i64,

pub struct Payload {
    pub service_flag: u8,
    pub seqno: u32,
    pub creation: time::OffsetDateTime,
}

impl Payload {
    pub fn new(seqno: u32) -> Self {
        Self {
            service_flag: 1,
            seqno,
            creation: time::OffsetDateTime::now_utc(),
        }
    }

    /// This is the microsDTN and DTN2 format
    pub fn to_bin_fmt(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let timeval_sec = self.creation.unix_timestamp() as u32;
        let timeval_msec = self.creation.microsecond();

        buf.push(self.service_flag);
        buf.extend_from_slice(&self.seqno.to_be_bytes());
        // (4 bytes, unsigned integer)
        buf.extend_from_slice(&timeval_sec.to_be_bytes());
        // (4 bytes, unsigned integer)
        buf.extend_from_slice(&timeval_msec.to_be_bytes());
        buf
    }

    /// This is the ION format
    pub fn to_text_fmt(&self) -> String {
        let timeval_sec = self.creation.unix_timestamp();
        let timeval_nsec = self.creation.nanosecond();

        format!(
            "{} {} {} {}",
            self.service_flag,
            self.seqno,
            timeval_sec,  // %ld
            timeval_nsec  // %ld
        )
    }

    pub fn from_bin_fmt(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() < 12 {
            return Err(anyhow::anyhow!("Payload too short"));
        }

        let timeval_sec = u32::from_be_bytes(data[5..9].try_into()?);
        let timeval_msec = u32::from_be_bytes(data[9..13].try_into()?) * 1000;

        Ok(Self {
            service_flag: data[0],
            seqno: u32::from_be_bytes(data[1..5].try_into()?),
            creation: time::OffsetDateTime::from_unix_timestamp(timeval_sec as i64)?
                + time::Duration::microseconds(timeval_msec as i64),
        })
    }

    pub fn from_text_fmt(data: &str) -> anyhow::Result<Self> {
        let data = data.trim_matches('\0');
        let parts = data
            .split(' ')
            .filter(|s| !s.is_empty())
            .collect::<Vec<&str>>();
        if parts.len() != 4 {
            return Err(anyhow::anyhow!("Invalid payload: '{data}'"));
        }

        let timeval_sec = parts[2]
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid seconds field: {e}"))?;
        let timeval_nsec = parts[3]
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid nanoseconds field: {e}"))?;

        Ok(Self {
            service_flag: parts[0]
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid service flag: {e}"))?,
            seqno: parts[1]
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid seqno field: {e}"))?,
            creation: time::OffsetDateTime::from_unix_timestamp(timeval_sec)?
                + time::Duration::nanoseconds(timeval_nsec),
        })
    }
}

pub fn build_payload(
    args: &Command,
    seq_no: u32,
) -> anyhow::Result<(Box<[u8]>, time::OffsetDateTime)> {
    let mut builder =
        hardy_bpv7::builder::Builder::new(args.source.clone().unwrap(), args.destination.clone());

    if let Some(report_to) = &args.report_to {
        builder = builder.with_report_to(report_to.clone());
    }

    (args.lifetime().as_millis() <= u64::MAX as u128)
        .then_some(())
        .ok_or(anyhow::anyhow!(
            "Lifetime too long: {}!",
            humantime::format_duration(args.lifetime())
        ))?;

    builder = builder.with_lifetime(args.lifetime());

    let payload = Payload::new(seq_no);
    let data = match args.format {
        Format::Text => payload.to_text_fmt().into(),
        Format::Binary => payload.to_bin_fmt(),
    };

    Ok((
        builder
            .add_extension_block(hardy_bpv7::block::Type::Payload)
            .with_flags(hardy_bpv7::block::Flags {
                delete_bundle_on_failure: true,
                ..Default::default()
            })
            .build(&data)
            .build(
                payload
                    .creation
                    .try_into()
                    .trace_expect("Failed to convert creation time"),
            )
            .1,
        payload.creation,
    ))
}
