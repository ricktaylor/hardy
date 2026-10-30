//! The output doors' hold on a stored bundle: [`pull_headers`] pulls the load
//! stream until the bytes held reach the payload's data, and any declared
//! peek of the payload after it, and the
//! [`LoadedBundle`] it returns replays the stored bundle from its first byte,
//! as stored or under [`RebuiltHeaders`](crate::rebuilt_headers::RebuiltHeaders).
//! The payload is the last block (RFC 9171 §4.1), so every block the output
//! stages edit is then resident. [`pull_whole`] pulls the rest, for a door
//! that needs the bundle whole.

use super::*;
use crate::{bundle::arrival::peekable_payload, stream::RecvError};

/// A stored bundle as the egress door holds it: `resident`, a prefix of the
/// stored bundle that reaches the payload's data, and the rest of the load
/// stream, until its `Final` is in hand.
///
/// As a [`Receiver`] it yields the stored bundle from its first byte: the
/// resident prefix as one segment, then the rest of the load stream.
pub struct LoadedBundle {
    pub resident: Bytes,
    rest: Option<Box<dyn Receiver<Segment>>>,
    // The resident prefix has been yielded.
    replayed: bool,
}

#[async_trait]
impl Receiver<Segment> for LoadedBundle {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        if !self.replayed {
            self.replayed = true;
            let resident = self.resident.clone();
            return Ok(match self.rest {
                Some(_) => Segment::Next(resident),
                None => Segment::Final(resident),
            });
        }
        let Some(rest) = self.rest.as_mut() else {
            return Err(RecvError);
        };
        let segment = rest.recv().await?;
        if matches!(segment, Segment::Final(_)) {
            self.rest = None;
        }
        Ok(segment)
    }
}

/// Pulls `stream` until the bytes held reach `bundle`'s payload data and the
/// payload's first min(`peek`, payload length) bytes after it. A payload with
/// no peekable prefix (`peekable_payload`: one a BCB covers) holds no peek.
/// When the first segment gets there, as a whole-buffer load's one `Final`
/// does, it is held as-is; only a region split across segments is gathered
/// into one buffer.
///
/// # Panics
///
/// Panics if the stream ends before the bytes held get there: the store holds
/// only bundles validated at a door or built by the BPA, so a short load is
/// storage corruption.
pub async fn pull_headers(
    bundle: &Bpv7Bundle,
    mut stream: Box<dyn Receiver<Segment>>,
    peek: usize,
) -> LoadedBundle {
    let payload = bundle
        .blocks
        .get(&1)
        .trace_expect("A stored bundle has a payload block")
        .payload_range();
    // The peek in u64, capped at the payload's length: both are wire-derived.
    let held_peek =
        peekable_payload(bundle).map_or(0, |_| (peek as u64).min(payload.end - payload.start));
    let hold_to = payload.start + held_peek;

    let mut first: Option<Bytes> = None;
    let mut gathered: Option<BytesMut> = None;
    let loaded = loop {
        let (data, last) = match stream
            .recv()
            .await
            .trace_expect("The bundle's load stream ended before its headers")
        {
            Segment::Next(data) => (data, false),
            Segment::Final(data) => (data, true),
        };

        let held = if let Some(current) = gathered.as_mut() {
            current.extend_from_slice(&data);
            current.len()
        } else if let Some(head) = first.take() {
            let mut current = match head.try_into_mut() {
                Ok(head) => head,
                Err(head) => {
                    let mut current = BytesMut::with_capacity(head.len() + data.len());
                    current.extend_from_slice(&head);
                    current
                }
            };
            current.extend_from_slice(&data);
            let held = current.len();
            gathered = Some(current);
            held
        } else {
            let held = data.len();
            first = Some(data);
            held
        };

        if held as u64 >= hold_to {
            let resident = match (first, gathered) {
                (Some(bytes), None) => bytes,
                (None, Some(buffer)) => buffer.freeze(),
                _ => unreachable!("exactly one accumulator holds the pulled bytes"),
            };
            break Some(LoadedBundle {
                resident,
                rest: (!last).then_some(stream),
                replayed: false,
            });
        }
        if last {
            break None;
        }
    };
    loaded.trace_expect("The stored bundle ends before its headers")
}

/// Pulls `loaded`'s load stream on to its `Final`, so the whole stored bundle
/// is in hand. A load already held whole, as a whole-buffer load's one
/// `Final` is, comes back as-is.
///
/// # Panics
///
/// Panics if the stream ends before its `Final`: the store holds only
/// bundles validated at a door or built by the BPA, so a short load is
/// storage corruption.
pub async fn pull_whole(loaded: LoadedBundle) -> Bytes {
    let LoadedBundle { resident, rest, .. } = loaded;
    let Some(mut rest) = rest else {
        return resident;
    };
    let mut whole = BytesMut::from(&resident[..]);
    loop {
        match rest
            .recv()
            .await
            .trace_expect("The bundle's load stream ended before its Final")
        {
            Segment::Next(data) => whole.extend_from_slice(&data),
            Segment::Final(data) => {
                whole.extend_from_slice(&data);
                return whole.freeze();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_async::channel::bounded;
    use hardy_bpv7::{
        block::Type,
        bpsec::{DecryptingReader, key::KeySet},
        builder::Builder,
        creation_timestamp::CreationTimestamp,
        editor::{Chunk, Editor},
        reader::{Availability, Reader},
    };
    use hardy_cbor::encode::emit;

    use super::*;
    use crate::rebuilt_headers::RebuiltHeaders;

    // A stored bundle carrying `payload`, and its block index.
    fn stored(payload: &[u8]) -> (Bpv7Bundle, Bytes) {
        let (bundle, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.2".parse().unwrap())
                .with_payload(payload.to_vec().into())
                .build(CreationTimestamp::now())
                .unwrap();
        (bundle, Bytes::from(data))
    }

    fn payload_start(bundle: &Bpv7Bundle) -> usize {
        usize::try_from(bundle.blocks[&1].payload_range().start).unwrap()
    }

    // The stored bundle's load stream, cut at `splits`, ending in its Final.
    async fn load(data: &Bytes, splits: &[usize]) -> Box<dyn Receiver<Segment>> {
        let (tx, rx) = bounded(splits.len() + 1);
        let mut start = 0;
        for &split in splits {
            tx.send(Segment::Next(data.slice(start..split)))
                .await
                .unwrap();
            start = split;
        }
        tx.send(Segment::Final(data.slice(start..))).await.unwrap();
        Box::new(rx)
    }

    // An output attempt's one rebuild: a Previous Node insert, so the chunks
    // mix new bytes with kept extents.
    fn rebuild(bundle: &Bpv7Bundle, source: &[u8]) -> Vec<Chunk> {
        let node: Eid = "ipn:0.9.0".parse().unwrap();
        let mut editor = Editor::new(bundle, source);
        editor
            .insert_block(Type::PreviousNode)
            .unwrap()
            .with_data(emit(&node).0.into())
            .rebuild();
        editor.rebuild().unwrap()
    }

    async fn drain(stream: &mut dyn Receiver<Segment>) -> Vec<Segment> {
        let mut segments = Vec::new();
        loop {
            let segment = stream.recv().await.unwrap();
            let last = matches!(segment, Segment::Final(_));
            segments.push(segment);
            if last {
                return segments;
            }
        }
    }

    fn concat(segments: &[Segment]) -> Vec<u8> {
        segments
            .iter()
            .flat_map(|(Segment::Next(data) | Segment::Final(data))| data.iter().copied())
            .collect()
    }

    // The rebuild's length and segments over the bytes `pull_headers` held.
    async fn output(bundle: &Bpv7Bundle, mut loaded: LoadedBundle) -> (u64, Vec<Segment>) {
        let chunks = rebuild(bundle, &loaded.resident);
        let head = loaded.resident.clone();
        let mut stream = RebuiltHeaders::new(chunks, &head, &mut loaded);
        let total_len = stream.total_len(bundle.encoded_len());
        (total_len, drain(&mut stream).await)
    }

    #[tokio::test]
    async fn a_whole_buffer_load_is_held_as_is() {
        let (bundle, data) = stored(b"held as loaded");
        let loaded = pull_headers(&bundle, Box::new(data.clone()), 0).await;
        assert_eq!(loaded.resident.as_ptr(), data.as_ptr());
        assert_eq!(loaded.resident.len(), data.len());
        assert!(loaded.rest.is_none());
    }

    // Wherever the load is cut, the pull holds through the headers and the
    // segments carry the bytes the flattened rebuild of the whole bundle
    // would: a header region split across two and three segments (gathered),
    // a cut exactly at the payload's data and inside it, and an empty Final
    // after the whole bundle.
    #[tokio::test]
    async fn every_cut_streams_the_flattened_rebuild() {
        let (bundle, data) = stored(&[0x5A; 64]);
        let start = payload_start(&bundle);
        let expected = Chunk::flatten(rebuild(&bundle, &data), &data);

        for (splits, held, shared) in [
            (vec![], data.len(), true),
            (vec![10, start + 10], start + 10, false),
            (vec![5, 10, start + 10], start + 10, false),
            (vec![start], start, true),
            (vec![start + 3], start + 3, true),
            (vec![data.len()], data.len(), true),
        ] {
            let loaded = pull_headers(&bundle, load(&data, &splits).await, 0).await;
            assert_eq!(loaded.resident.len(), held, "cut at {splits:?}");
            assert_eq!(
                loaded.resident.as_ptr() == data.as_ptr(),
                shared,
                "cut at {splits:?}: only a split header region is gathered"
            );
            assert_eq!(loaded.rest.is_some(), !splits.is_empty());

            let (total_len, segments) = output(&bundle, loaded).await;
            assert_eq!(concat(&segments), &*expected, "cut at {splits:?}");
            assert_eq!(total_len, expected.len() as u64, "cut at {splits:?}");
        }
    }

    // Past its Final, a loaded bundle reports the stream ended, as a channel
    // whose sender is gone does.
    #[tokio::test]
    async fn a_loaded_bundle_ends_after_its_final() {
        let (bundle, data) = stored(b"replayed once");
        let start = payload_start(&bundle);
        let mut loaded = pull_headers(&bundle, load(&data, &[start]).await, 0).await;
        assert_eq!(concat(&drain(&mut loaded).await), &*data);
        assert!(matches!(loaded.recv().await, Err(RecvError)));
    }

    // The rebuilt header region travels as one segment, and the kept payload
    // block as a slice of the stored allocation.
    #[tokio::test]
    async fn the_payload_shares_the_stored_allocation() {
        let (bundle, data) = stored(b"zero-copy payload");
        let loaded = pull_headers(&bundle, Box::new(data.clone()), 0).await;
        let (_, segments) = output(&bundle, loaded).await;
        let [Segment::Next(head), Segment::Final(payload)] = segments.as_slice() else {
            panic!("the rebuilt headers, then the payload block: {segments:?}");
        };
        let stored_range = data.as_ptr_range();
        assert!(
            !stored_range.contains(&head.as_ptr()),
            "the head is rebuilt"
        );
        assert!(stored_range.contains(&payload.as_ptr()));
    }

    // A Rewriter, reading through the decrypting reader over the attempt's
    // editor, sees the payload only while it is resident.
    #[tokio::test]
    async fn a_payload_past_the_resident_end_reads_not_resident() {
        let (bundle, data) = stored(&[0xA5; 64]);
        let start = payload_start(&bundle);
        let no_bcbs = HashMap::new();
        let no_keys = KeySet::EMPTY;
        for (splits, resident_payload) in [
            (vec![], true),
            (vec![start], false),
            (vec![start + 3], false),
        ] {
            let loaded = pull_headers(&bundle, load(&data, &splits).await, 0).await;
            let editor = Editor::new(&bundle, &loaded.resident);
            let current = editor.current_bundle();
            let reader =
                DecryptingReader::over_editor(&current.blocks, &editor, &no_bcbs, &no_keys);
            let (_, availability) = reader.block(1).unwrap();
            if resident_payload {
                assert!(
                    matches!(availability, Availability::Available(_)),
                    "cut at {splits:?}: the payload is resident"
                );
            } else {
                assert!(
                    matches!(availability, Availability::NotResident),
                    "cut at {splits:?}: the payload is past the resident end"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_empty_payload_streams() {
        let (bundle, data) = stored(b"");
        let expected = Chunk::flatten(rebuild(&bundle, &data), &data);
        let loaded = pull_headers(&bundle, Box::new(data.clone()), 0).await;
        let (total_len, segments) = output(&bundle, loaded).await;
        assert_eq!(concat(&segments), &*expected);
        assert_eq!(total_len, expected.len() as u64);
    }

    // An attempt that edited nothing hands on the stored bundle as it was
    // loaded, wherever the load is cut, ending in its Final.
    #[tokio::test]
    async fn an_unedited_bundle_streams_as_stored() {
        let (bundle, data) = stored(&[0x6B; 64]);
        let start = payload_start(&bundle);
        for splits in [vec![], vec![10, start + 10], vec![start], vec![start + 3]] {
            let mut loaded = pull_headers(&bundle, load(&data, &splits).await, 0).await;
            assert_eq!(
                concat(&drain(&mut loaded).await),
                &*data,
                "cut at {splits:?}"
            );
        }
    }

    // A declared peek holds the payload's first bytes as well as the headers,
    // wherever the load is cut, capped at the payload's length; no peek holds
    // nothing past the headers.
    #[tokio::test]
    async fn a_peek_holds_the_payload_prefix() {
        let (bundle, data) = stored(&[0x4E; 64]);
        let start = payload_start(&bundle);
        let end = start + 64;
        for (splits, peek, held) in [
            (vec![start, start + 4, start + 12], 0, start),
            (vec![start, start + 4, start + 12], 8, start + 12),
            (vec![start + 8], 8, start + 8),
            (vec![10, start + 40, end], 100, end),
        ] {
            let loaded = pull_headers(&bundle, load(&data, &splits).await, peek).await;
            assert_eq!(
                loaded.resident.len(),
                held,
                "peek {peek}, cut at {splits:?}"
            );
            assert_eq!(&loaded.resident[..], &data[..held]);
            assert!(loaded.rest.is_some(), "the rest still streams");
        }
    }

    // A door that needs the bundle whole pulls the load on to its Final,
    // wherever it is cut; a load held whole comes back zero-copy.
    #[tokio::test]
    async fn pull_whole_holds_the_whole_stored_bundle() {
        let (bundle, data) = stored(&[0x2D; 64]);
        let start = payload_start(&bundle);
        for (splits, shared) in [
            (vec![], true),
            (vec![10, start + 10], false),
            (vec![start], false),
            (vec![start + 3], false),
        ] {
            let loaded = pull_headers(&bundle, load(&data, &splits).await, 0).await;
            let whole = pull_whole(loaded).await;
            assert_eq!(whole, data, "cut at {splits:?}");
            assert_eq!(
                whole.as_ptr() == data.as_ptr(),
                shared,
                "cut at {splits:?}: only a load held whole is returned as-is"
            );
        }
    }

    #[tokio::test]
    #[should_panic(expected = "ended before its Final")]
    async fn a_load_lost_before_its_final_is_fatal() {
        let (bundle, data) = stored(b"lost after the headers");
        let start = payload_start(&bundle);
        let (tx, rx) = bounded(1);
        tx.send(Segment::Next(data.slice(..start + 2)))
            .await
            .unwrap();
        drop(tx);
        pull_whole(pull_headers(&bundle, Box::new(rx), 0).await).await;
    }

    #[tokio::test]
    #[should_panic(expected = "ends before its headers")]
    async fn a_stored_bundle_short_of_its_headers_is_fatal() {
        let (bundle, data) = stored(b"cut short");
        pull_headers(&bundle, Box::new(data.slice(..10)), 0).await;
    }

    #[tokio::test]
    #[should_panic(expected = "ended before its headers")]
    async fn a_load_lost_before_the_headers_is_fatal() {
        let (bundle, data) = stored(b"lost");
        let (tx, rx) = bounded(1);
        tx.send(Segment::Next(data.slice(..10))).await.unwrap();
        drop(tx);
        pull_headers(&bundle, Box::new(rx), 0).await;
    }
}
