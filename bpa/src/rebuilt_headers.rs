//! A bundle's bytes with its header region rebuilt and its payload passed
//! through untouched.
//!
//! The payload is the bundle's last block (RFC 9171 §4.1), so every header
//! precedes it: a stage that edits only headers needs only the bytes up to the
//! payload resident, and an editor's rebuild over them ends with the payload
//! block kept as it stands. [`RebuiltHeaders`] yields the rebuilt header region
//! as one segment, then the bundle's own bytes from where that last kept chunk
//! starts, as they stream, so the payload is never held whole.

use alloc::vec::Vec;

use hardy_bpv7::editor::Chunk;

use crate::{
    Bytes, async_trait,
    cla::Segment,
    stream::{Receiver, RecvError},
};

/// A bundle stream whose header region is replaced by an editor's rebuild and
/// whose payload passes through as the inner stream yields it.
pub struct RebuiltHeaders<'a> {
    // The rebuilt header region, until it is yielded.
    head: Option<Bytes>,
    // How many of the inner stream's bytes are still to be dropped: the region
    // the rebuilt head replaces.
    skip: usize,
    // The bundle's bytes from its first byte.
    stream: &'a mut dyn Receiver<Segment>,
    // The inner stream's `Final` has been yielded.
    done: bool,
}

impl<'a> RebuiltHeaders<'a> {
    /// `chunks` is an editor's rebuild ([`Editor::rebuild`] or
    /// [`Editor::rebuild_bundle`]) over `resident`, the bundle's first bytes,
    /// covering every header; `stream` yields the bundle's bytes from its first
    /// byte, `resident` included. The rebuild's last chunk keeps the payload
    /// block as it stands, so every byte from that chunk's start passes through
    /// from `stream`.
    ///
    /// # Panics
    ///
    /// Panics if the rebuild's last chunk is not kept, since a rebuilt payload
    /// cannot stream, or if a kept chunk before it lies beyond `resident`.
    ///
    /// [`Editor::rebuild`]: hardy_bpv7::editor::Editor::rebuild
    /// [`Editor::rebuild_bundle`]: hardy_bpv7::editor::Editor::rebuild_bundle
    pub fn new(
        mut chunks: Vec<Chunk>,
        resident: &[u8],
        stream: &'a mut dyn Receiver<Segment>,
    ) -> Self {
        let Some(Chunk::Unchanged(kept)) = chunks.pop() else {
            panic!("the rebuild keeps the payload block as it stands");
        };
        Self {
            head: Some(Chunk::flatten_head(chunks, resident).into()),
            skip: kept.start,
            stream,
            done: false,
        }
    }
}

#[async_trait]
impl Receiver<Segment> for RebuiltHeaders<'_> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        if let Some(head) = self.head.take() {
            return Ok(Segment::Next(head));
        }
        if self.done {
            return Err(RecvError);
        }
        loop {
            let (mut bytes, last) = match self.stream.recv().await? {
                Segment::Next(bytes) => (bytes, false),
                Segment::Final(bytes) => (bytes, true),
            };
            if self.skip > 0 {
                let dropped = self.skip.min(bytes.len());
                bytes = bytes.slice(dropped..);
                self.skip -= dropped;
                assert!(
                    self.skip == 0 || !last,
                    "the bundle's stream ended inside its own header region"
                );
            }
            if last {
                self.done = true;
                return Ok(Segment::Final(bytes));
            }
            if !bytes.is_empty() {
                return Ok(Segment::Next(bytes));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU8;

    use hardy_bpv7::{
        Bundle as Bpv7Bundle, block,
        builder::Builder,
        creation_timestamp::CreationTimestamp,
        editor::Editor,
        hop_info::HopInfo,
        parse::{Parsed, parse},
    };
    use hardy_cbor::encode::emit;

    use super::*;

    // A bundle with a Hop Count block before a payload of `payload_len` bytes.
    fn bundle(payload_len: usize) -> (Bpv7Bundle, Bytes) {
        let (_, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_hop_count(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .with_payload(vec![0xAB; payload_len].into())
            .build(CreationTimestamp::now())
            .unwrap();
        let Parsed { bundle, data, .. } = parse(Bytes::from(data)).unwrap();
        (bundle, data)
    }

    fn hop_count(bundle: &Bpv7Bundle) -> u64 {
        *bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::HopCount)
            .expect("the fixture carries a Hop Count")
            .0
    }

    // The bundle as a stream of `cuts` segments, the last one `Final`.
    async fn stream_of(data: &Bytes, cuts: &[usize]) -> hardy_async::channel::Receiver<Segment> {
        let mut bounds = vec![0];
        bounds.extend_from_slice(cuts);
        bounds.push(data.len());
        let (tx, rx) = hardy_async::channel::bounded(bounds.len());
        for (i, pair) in bounds.windows(2).enumerate() {
            let bytes = data.slice(pair[0]..pair[1]);
            let segment = if i == bounds.len() - 2 {
                Segment::Final(bytes)
            } else {
                Segment::Next(bytes)
            };
            tx.send(segment).await.unwrap();
        }
        rx
    }

    async fn drain(stream: &mut impl Receiver<Segment>) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match stream.recv().await.expect("the stream runs to its Final") {
                Segment::Next(bytes) => out.extend_from_slice(&bytes),
                Segment::Final(bytes) => {
                    out.extend_from_slice(&bytes);
                    assert!(
                        matches!(stream.recv().await, Err(RecvError)),
                        "nothing follows the Final"
                    );
                    return out;
                }
            }
        }
    }

    // For a stream cut at every position, the rebuilt stream carries exactly
    // the bundle the editor's whole rebuild flattens to.
    async fn streams_the_flattened_rebuild(
        payload_len: usize,
        edit: impl Fn(Editor<'_>, u64) -> Editor<'_>,
    ) {
        let (bundle, data) = bundle(payload_len);
        let hop_count = hop_count(&bundle);
        let expected = Chunk::flatten(
            edit(Editor::new(&bundle, &data), hop_count)
                .rebuild()
                .unwrap(),
            &data,
        );
        for cut in 1..data.len() {
            let chunks = edit(Editor::new(&bundle, &data), hop_count)
                .rebuild()
                .unwrap();
            let mut stream = stream_of(&data, &[cut]).await;
            let mut rebuilt = RebuiltHeaders::new(chunks, &data, &mut stream);
            assert_eq!(drain(&mut rebuilt).await, expected.as_ref(), "cut at {cut}");
        }
    }

    fn update_hop_count(mut editor: Editor<'_>, hop_count: u64) -> Editor<'_> {
        editor
            .update_block(hop_count)
            .unwrap()
            .with_data(
                emit(&HopInfo {
                    limit: NonZeroU8::new(64).unwrap(),
                    count: 2,
                })
                .0
                .into(),
            )
            .rebuild();
        editor
    }

    fn remove_hop_count(mut editor: Editor<'_>, hop_count: u64) -> Editor<'_> {
        editor.remove_block(hop_count).unwrap();
        editor
    }

    #[tokio::test]
    async fn an_updated_header_streams_at_every_cut() {
        streams_the_flattened_rebuild(300, update_hop_count).await;
    }

    #[tokio::test]
    async fn a_removed_header_streams_at_every_cut() {
        streams_the_flattened_rebuild(300, remove_hop_count).await;
    }

    #[tokio::test]
    async fn an_empty_payload_streams_at_every_cut() {
        streams_the_flattened_rebuild(0, remove_hop_count).await;
    }

    // With nothing edited the rebuild keeps everything after the array head,
    // so the stream is the bundle itself.
    #[tokio::test]
    async fn an_unedited_bundle_streams_unchanged() {
        streams_the_flattened_rebuild(300, |editor, _| editor).await;
    }

    // A stream of one-byte segments: the replaced region spans many of them,
    // and the empty remainders it leaves are not yielded.
    #[tokio::test]
    async fn the_replaced_region_spans_segments() {
        let (bundle, data) = bundle(64);
        let hop_count = hop_count(&bundle);
        let expected = Chunk::flatten(
            remove_hop_count(Editor::new(&bundle, &data), hop_count)
                .rebuild()
                .unwrap(),
            &data,
        );
        let chunks = remove_hop_count(Editor::new(&bundle, &data), hop_count)
            .rebuild()
            .unwrap();
        let cuts: Vec<usize> = (1..data.len()).collect();
        let mut stream = stream_of(&data, &cuts).await;
        let mut rebuilt = RebuiltHeaders::new(chunks, &data, &mut stream);
        let mut out = Vec::new();
        loop {
            match rebuilt.recv().await.unwrap() {
                Segment::Next(bytes) => {
                    assert!(!bytes.is_empty(), "no empty segment is yielded");
                    out.extend_from_slice(&bytes);
                }
                Segment::Final(bytes) => {
                    out.extend_from_slice(&bytes);
                    break;
                }
            }
        }
        assert_eq!(out, expected.as_ref());
    }

    // The editor sees only the bytes up to the payload, as the ingress gate
    // holds them, and the payload streams on behind the rebuilt headers.
    #[tokio::test]
    async fn only_the_headers_need_be_resident() {
        let (bundle, data) = bundle(300);
        let hop_count = hop_count(&bundle);
        let expected = Chunk::flatten(
            remove_hop_count(Editor::new(&bundle, &data), hop_count)
                .rebuild()
                .unwrap(),
            &data,
        );
        let payload_start = usize::try_from(bundle.blocks[&1].extent.start).unwrap();
        let resident = data.slice(..payload_start);
        let chunks = remove_hop_count(Editor::new(&bundle, &resident), hop_count)
            .rebuild()
            .unwrap();
        let mut stream = stream_of(&data, &[payload_start, payload_start + 100]).await;
        let mut rebuilt = RebuiltHeaders::new(chunks, &resident, &mut stream);
        assert_eq!(drain(&mut rebuilt).await, expected.as_ref());
    }

    #[test]
    #[should_panic(expected = "the rebuild keeps the payload block as it stands")]
    fn a_rebuilt_payload_cannot_stream() {
        let (bundle, data) = bundle(16);
        let mut editor = Editor::new(&bundle, &data);
        editor
            .update_block(1)
            .unwrap()
            .with_data(b"replaced".as_slice().into())
            .rebuild();
        let chunks = editor.rebuild().unwrap();
        let mut stream = data.clone();
        let _ = RebuiltHeaders::new(chunks, &data, &mut stream);
    }
}
