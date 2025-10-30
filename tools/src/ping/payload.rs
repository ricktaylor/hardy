#[repr(C)]
pub struct Payload {
    /*
     * (1 byte)
     * A "service" or "type" flag.
     * 0x01 = PING (This is what bping sends)
     * 0x02 = PONG (This is what the reply service sends back)
     */
    pub service_flag: u8,

    /*
     * (4 bytes, unsigned integer)
     * The sequence number of this specific ping packet,
     * starting from 0 or 1.
     */
    pub seqno: u32,

    /*
     * The time the ping was sent (seconds part).
     * This is the 'sec' field from a timeval struct.
     */
    pub timeval_sec: i64,

    /*
     * The time the ping was sent (microseconds or nanoseconds part).
     * This is the 'usec' or 'nsec' field from a timeval/timespec struct.
     * The bping client and pong server must agree on the unit.
     */
    pub timeval_nsec: i64,
}

impl Payload {
    pub fn new(seqno: u32) -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            service_flag: 1,
            seqno,
            timeval_sec: now.unix_timestamp(),
            timeval_nsec: now.nanosecond() as i64,
        }
    }

    /// This is the microsDTN and DTN2 format
    pub fn to_bin_fmt(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(self.service_flag);
        buf.extend_from_slice(&self.seqno.to_be_bytes());
        // (4 bytes, unsigned integer)
        buf.extend_from_slice(&(self.timeval_sec as u32).to_be_bytes());
        // (4 bytes, unsigned integer)
        buf.extend_from_slice(&(self.timeval_nsec as u32).to_be_bytes());
        buf
    }

    /// This is the ION format
    pub fn to_text_fmt(&self) -> String {
        format!(
            "{} {} {} {}",
            self.service_flag,
            self.seqno,
            self.timeval_sec,  // %ld
            self.timeval_nsec  // %ld
        )
    }

    pub fn from_bin_fmt(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() < 12 {
            return Err(anyhow::anyhow!("Payload too short"));
        }

        Ok(Self {
            service_flag: data[0],
            seqno: u32::from_be_bytes(data[1..5].try_into().unwrap()),
            timeval_sec: u32::from_be_bytes(data[5..9].try_into().unwrap()) as i64,
            timeval_nsec: (u32::from_be_bytes(data[9..13].try_into().unwrap()) as i64) * 1000,
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

        Ok(Self {
            service_flag: parts[0]
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid service flag: {e}"))?,
            seqno: parts[1]
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid seqno field: {e}"))?,
            timeval_sec: parts[2]
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid seconds field: {e}"))?,
            timeval_nsec: parts[3]
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid nanoseconds field: {e}"))?,
        })
    }
}
