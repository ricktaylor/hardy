use super::*;
use hardy_bpv7::status_report::ReasonCode;
use std::collections::HashSet;

impl Rib {
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub fn find(&self, to: &Eid) -> Result<Option<FindResult>, Option<ReasonCode>> {
        find_recurse(
            &self.inner.read().trace_expect("Failed to lock mutex"),
            to,
            &mut HashSet::new(),
        )
    }
}

#[cfg_attr(feature = "tracing", instrument(skip(inner)))]
fn find_local<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<FindResult> {
    let mut clas: Option<Vec<u32>> = None;
    for action in inner.locals.actions.get(to).into_iter().flatten() {
        match &action {
            local::Action::AdminEndpoint => {
                return Some(FindResult::AdminEndpoint);
            }
            local::Action::Local(service) => {
                return Some(FindResult::Deliver(service.clone()));
            }
            local::Action::Forward(peer_id) => {
                if let Some(clas) = &mut clas {
                    clas.push(*peer_id);
                } else {
                    clas = Some(vec![*peer_id]);
                }
            }
        }
    }
    clas.map(|clas| FindResult::Forward(clas, false))
}

#[cfg_attr(feature = "tracing", instrument(skip(inner, trail)))]
fn find_recurse<'a>(
    inner: &'a RibInner,
    to: &'a Eid,
    trail: &mut HashSet<&'a Eid>,
) -> Result<Option<FindResult>, Option<ReasonCode>> {
    // Recursion check
    if !trail.insert(to) {
        warn!("Recursive route {to} found!");
        return Err(Some(ReasonCode::NoKnownRouteToDestinationFromHere));
    }

    // Always check locals first
    let mut result = find_local(inner, to);
    if result.is_some() {
        return Ok(result);
    }

    // Now check routes (this is where route table switching can occur)

    let mut priority = None;
    for entry in inner
        .routes
        .find(to)
        .collect::<std::collections::BinaryHeap<_>>()
    {
        // Ensure we only look at lowest priority values
        if let Some(priority) = priority {
            if entry.priority > priority {
                break;
            }
        } else {
            priority = Some(entry.priority);
        }

        match &entry.action {
            routes::Action::Drop(reason) => {
                // Drop trumps everything else
                return Err(*reason);
            }
            routes::Action::Reflect => {
                if let Some(FindResult::Forward(_, reflect)) = &mut result {
                    *reflect = true;
                } else {
                    result = Some(FindResult::Forward(Vec::new(), true));
                }
            }
            routes::Action::Via(via) => {
                // Recursive lookup
                if let Some(sub_result) = find_recurse(inner, via, trail)? {
                    match sub_result {
                        FindResult::AdminEndpoint | FindResult::Deliver(_) => {
                            // If we find a non-forward, then break
                            result = Some(sub_result);
                            break;
                        }
                        FindResult::Forward(sub_clas, sub_reflect) => {
                            // Append clas to the running set of clas
                            if let Some(FindResult::Forward(clas, reflect)) = &mut result {
                                clas.extend(sub_clas);
                                *reflect = *reflect || sub_reflect;
                            } else {
                                result = Some(FindResult::Forward(sub_clas, sub_reflect));
                            }
                        }
                    }
                } else {
                    // TODO: Kick off a resolver lookup for `via`
                }
            }
        }

        trail.remove(to);
    }

    if result.is_none() && inner.locals.finals.contains(to) {
        return Err(Some(ReasonCode::DestinationEndpointIDUnavailable));
    }
    Ok(result)
}
