use super::*;
use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId};

pub struct ArbitraryEid(pub Eid);

impl<'a> Arbitrary<'a> for ArbitraryEid {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        if u.arbitrary::<bool>()? {
            let allocator_id = u.arbitrary()?;
            let node_number = u.arbitrary()?;
            let service_number = u.arbitrary()?;

            if allocator_id == 0 && node_number == 0 && service_number == 0 {
                Ok(ArbitraryEid(Eid::Null))
            } else if allocator_id == 0 && node_number == u32::MAX {
                Ok(ArbitraryEid(Eid::LocalNode(service_number)))
            } else {
                Ok(ArbitraryEid(Eid::Ipn {
                    fqnn: IpnNodeId {
                        allocator_id,
                        node_number,
                    },
                    service_number,
                }))
            }
        } else {
            let node_name: Box<str> = urlencoding::decode(u.arbitrary()?)
                .map_err(|_| arbitrary::Error::IncorrectFormat)?
                .into();
            if node_name.as_ref() == "none" {
                Ok(ArbitraryEid(Eid::Null))
            } else {
                let demux: String = u.arbitrary()?;
                if demux.contains(|c| ('\u{21}'..='\u{7e}').contains(&c)) {
                    Err(arbitrary::Error::IncorrectFormat)
                } else {
                    Ok(ArbitraryEid(Eid::Dtn {
                        node_name: DtnNodeId { node_name },
                        service_name: demux.into(),
                    }))
                }
            }
        }
    }
}
