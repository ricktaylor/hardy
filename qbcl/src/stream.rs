//! Stream content framing (Section 4.3.2): each reliable transfer is one unidirectional stream carrying exactly one bundle, either directly or as the content of one definite-length CBOR byte string.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

/// A specialized `Result` type for stream framing.
pub type Result<T> = core::result::Result<T, Error>;

/// A stream framing fault (Section 4.3.2). Every variant is a protocol error, on which the receiver closes the connection with [`ErrorCode::ProtocolError`](crate::error_code::ErrorCode::ProtocolError).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    /// The stream ended without any data.
    #[error("the stream ended without any data")]
    Empty,

    /// The stream's first octet is neither a recognized bundle version octet nor the head of a definite-length byte string.
    #[error("first octet {0:#04x} is neither a bundle version octet nor a byte string head")]
    UnknownFirstOctet(u8),

    /// The stream begins with an indefinite-length byte string, which does not declare the bundle's length.
    #[error("an indefinite-length byte string does not declare the bundle's length")]
    IndefiniteLength,

    /// The stream ended inside the byte string head.
    #[error("the stream ended inside the byte string head")]
    TruncatedHead,

    /// The byte string is empty, so its content does not begin with a bundle version octet.
    #[error("the byte string is empty")]
    EmptyContent,

    /// The byte string's content does not begin with a recognized bundle version octet.
    #[error("the byte string's content begins with {0:#04x}, not a bundle version octet")]
    UnknownVersionOctet(u8),

    /// Data continued past the byte string's declared length.
    #[error("data continued past the {declared} octets declared")]
    ExcessData {
        /// The declared content length, in octets.
        declared: u64,
    },

    /// The stream ended before the byte string's declared length was received.
    #[error("the stream ended after {received} of the {declared} octets declared")]
    Truncated {
        /// The declared content length, in octets.
        declared: u64,
        /// The content octets received.
        received: u64,
    },
}

/// The first octet of a BPv7 bundle: a CBOR indefinite-length array (RFC 9171 Section 4.1).
const BPV7_OCTET: u8 = 0x9f;

/// The first octet of a BPv6 bundle: its version number (RFC 5050).
const BPV6_OCTET: u8 = 0x06;

/// CBOR major type 2, a byte string, in the top three bits of a head's first octet (RFC 8949 Section 3.1).
const BYTE_STRING: u8 = 0x40;

/// The first octet of an indefinite-length byte string (RFC 8949 Section 3.2.3).
const INDEFINITE_BYTE_STRING: u8 = 0x5f;

/// The longest byte string head: the first octet and an 8-octet length.
const MAX_HEAD_LEN: usize = 9;

/// A bundle version that Table 1 recognizes by its first octet.
///
/// Recognizing a version is framing; supporting it is policy. A receiver that recognizes but does not support a version cancels the transfer with [`ErrorCode::UnsupportedBundleVersion`](crate::error_code::ErrorCode::UnsupportedBundleVersion) rather than treating it as a protocol error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BundleVersion {
    /// A BPv6 bundle (RFC 5050).
    V6,
    /// A BPv7 bundle (RFC 9171).
    V7,
}

impl BundleVersion {
    fn from_octet(octet: u8) -> Option<Self> {
        match octet {
            BPV7_OCTET => Some(Self::V7),
            BPV6_OCTET => Some(Self::V6),
            _ => None,
        }
    }
}

/// How a stream carries its bundle (Section 4.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Form {
    /// The bundle's own encoding, framed by the stream: its length is known only at FIN.
    Direct,
    /// The bundle's encoding as the content of a definite-length byte string.
    Wrapped {
        /// The declared length of the bundle, in octets.
        len: u64,
    },
}

/// What a stream's first octets say about the bundle it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamHead {
    /// The form the bundle is carried in.
    pub form: Form,
    /// The bundle's version, from its first octet.
    pub version: BundleVersion,
}

/// The result of [`StreamReader::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// The stream's head, reported once, by the push that completes it.
    pub head: Option<StreamHead>,
    /// The bundle octets in the pushed chunk, which may be none.
    pub data: Bytes,
}

/// Reads one stream's content, checking its framing as each chunk of stream data arrives.
///
/// The reader passes bundle octets through as slices of the chunks pushed, without copying them, and reports the [`StreamHead`] as soon as the first octets determine it: for the wrapped form that is the byte string head and the first content octet, so a receiver can apply its size and version policy before the bulk of the bundle arrives. [`Self::finish`] checks the framing at FIN.
///
/// After a push fails, the reader returns the same error from every later call.
#[derive(Debug, Default)]
pub struct StreamReader {
    state: State,
}

#[derive(Debug, Default)]
enum State {
    /// No data yet.
    #[default]
    Start,
    /// Part of a byte string head: the octets received so far, at least the first.
    Head {
        octets: [u8; MAX_HEAD_LEN],
        filled: usize,
    },
    /// A byte string head declaring `len` octets, awaiting the first content octet that gives the bundle version.
    Version { len: u64 },
    /// Bundle content: the wrapped form's declared length, if any, and the bundle octets received.
    Content {
        declared: Option<u64>,
        received: u64,
    },
    /// A framing fault, reported again by every later call.
    Failed(Error),
}

impl StreamReader {
    /// A reader for a new stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bundle octets received so far.
    pub fn received(&self) -> u64 {
        match self.state {
            State::Content { received, .. } => received,
            _ => 0,
        }
    }

    /// Reads the next chunk of stream data, returning the bundle octets it holds and, once, the stream's head.
    ///
    /// # Errors
    ///
    /// Any [`enum@Error`] other than those raised only at FIN: [`Error::Empty`], [`Error::TruncatedHead`], and [`Error::Truncated`].
    pub fn push(&mut self, mut chunk: Bytes) -> Result<Pushed> {
        match self.advance(&mut chunk) {
            Ok(head) => Ok(Pushed { head, data: chunk }),
            Err(e) => {
                self.state = State::Failed(e.clone());
                Err(e)
            }
        }
    }

    /// Checks the framing at FIN.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] if no data arrived, [`Error::TruncatedHead`] or [`Error::Truncated`] if the stream ended short of its byte string head or declared length, or the error an earlier push returned.
    pub fn finish(self) -> Result<()> {
        match self.state {
            State::Start => Err(Error::Empty),
            State::Head { .. } => Err(Error::TruncatedHead),
            State::Version { len } => Err(Error::Truncated {
                declared: len,
                received: 0,
            }),
            State::Content {
                declared: Some(declared),
                received,
            } if received < declared => Err(Error::Truncated { declared, received }),
            State::Content { .. } => Ok(()),
            State::Failed(e) => Err(e),
        }
    }

    // Consumes framing octets from the front of `chunk`, leaving only bundle
    // octets, and returns the head if this chunk completed it.
    fn advance(&mut self, chunk: &mut Bytes) -> Result<Option<StreamHead>> {
        let mut head = None;
        loop {
            match &mut self.state {
                State::Failed(e) => return Err(e.clone()),
                State::Content { declared, received } => {
                    // Lossless: usize is at most 64 bits on every target.
                    let len = chunk.len() as u64;
                    if let Some(declared) = *declared
                        && len > declared - *received
                    {
                        return Err(Error::ExcessData { declared });
                    }
                    *received += len;
                    return Ok(head);
                }
                // Checked before the empty-chunk guard: a 1-octet head completes
                // with no further data.
                State::Head { octets, filled } => {
                    let Some(len) = head_len(octets[0]) else {
                        unreachable!("only a byte string head's first octet starts a head");
                    };
                    let take = (len - *filled).min(chunk.len());
                    octets[*filled..*filled + take].copy_from_slice(&chunk[..take]);
                    chunk.advance(take);
                    *filled += take;
                    if *filled < len {
                        // The chunk ended inside the head.
                        return Ok(head);
                    }
                    match head_value(&octets[..len]) {
                        0 => return Err(Error::EmptyContent),
                        len => self.state = State::Version { len },
                    }
                }
                _ if chunk.is_empty() => return Ok(head),
                State::Start => {
                    let first = chunk[0];
                    if let Some(version) = BundleVersion::from_octet(first) {
                        // The direct form's first octet is the bundle's own.
                        head = Some(StreamHead {
                            form: Form::Direct,
                            version,
                        });
                        self.state = State::Content {
                            declared: None,
                            received: 0,
                        };
                    } else if head_len(first).is_some() {
                        let mut octets = [0; MAX_HEAD_LEN];
                        octets[0] = first;
                        chunk.advance(1);
                        self.state = State::Head { octets, filled: 1 };
                    } else if first == INDEFINITE_BYTE_STRING {
                        return Err(Error::IndefiniteLength);
                    } else {
                        return Err(Error::UnknownFirstOctet(first));
                    }
                }
                State::Version { len } => {
                    let octet = chunk[0];
                    let version = BundleVersion::from_octet(octet)
                        .ok_or(Error::UnknownVersionOctet(octet))?;
                    let len = *len;
                    head = Some(StreamHead {
                        form: Form::Wrapped { len },
                        version,
                    });
                    self.state = State::Content {
                        declared: Some(len),
                        received: 0,
                    };
                }
            }
        }
    }
}

/// The length of a definite-length byte string head with first octet `first`: the octet itself followed by 0, 1, 2, 4, or 8 octets of length (RFC 8949 Section 3), or `None` if `first` does not begin one.
fn head_len(first: u8) -> Option<usize> {
    match first.checked_sub(BYTE_STRING)? {
        0..24 => Some(1),
        24 => Some(2),
        25 => Some(3),
        26 => Some(5),
        27 => Some(9),
        _ => None,
    }
}

/// The length a complete byte string head declares (RFC 8949 Section 3).
fn head_value(head: &[u8]) -> u64 {
    match head {
        [first] => u64::from(first - BYTE_STRING),
        [_, len @ ..] => len
            .iter()
            .fold(0, |value, &octet| (value << 8) | u64::from(octet)),
        [] => unreachable!("a head has at least its first octet"),
    }
}

/// The head of a definite-length byte string of `len` octets, in the shortest encoding (RFC 8949 Sections 3 and 4.2.1), which a sender writes before a bundle in the wrapped form.
pub fn wrapped_head(len: u64) -> Bytes {
    let mut head = BytesMut::with_capacity(MAX_HEAD_LEN);
    // Each cast is lossless within its arm's range.
    match len {
        0..24 => head.put_u8(BYTE_STRING | len as u8),
        24..=0xff => {
            head.put_u8(BYTE_STRING | 24);
            head.put_u8(len as u8);
        }
        0x100..=0xffff => {
            head.put_u8(BYTE_STRING | 25);
            head.put_u16(len as u16);
        }
        0x1_0000..=0xffff_ffff => {
            head.put_u8(BYTE_STRING | 26);
            head.put_u32(len as u32);
        }
        _ => {
            head.put_u8(BYTE_STRING | 27);
            head.put_u64(len);
        }
    }
    head.freeze()
}

#[cfg(test)]
mod tests {
    use hardy_cbor::{
        decode::{self, Head, Marker},
        encode::{self, BytesHeader},
    };

    use super::*;

    // Lengths at each boundary between head sizes.
    const BOUNDARY_LENGTHS: [u64; 10] = [
        1,
        23,
        24,
        0xff,
        0x100,
        0xffff,
        0x1_0000,
        0xffff_ffff,
        0x1_0000_0000,
        u64::MAX,
    ];

    // A stand-in BPv7 bundle: the framing reads only its first octet.
    fn bpv7_bundle(len: usize) -> Bytes {
        let mut bundle = vec![BPV7_OCTET];
        bundle.extend((1..len).map(|i| i as u8));
        bundle.into()
    }

    fn wrapped(bundle: &Bytes) -> Bytes {
        [wrapped_head(bundle.len() as u64), bundle.clone()]
            .concat()
            .into()
    }

    // Pushes `chunks` in order and finishes, returning the heads reported
    // and the bundle octets passed through.
    fn read(chunks: impl IntoIterator<Item = Bytes>) -> Result<(Vec<StreamHead>, Vec<u8>)> {
        let mut reader = StreamReader::new();
        let mut heads = Vec::new();
        let mut data = Vec::new();
        for chunk in chunks {
            let pushed = reader.push(chunk)?;
            heads.extend(pushed.head);
            data.extend_from_slice(&pushed.data);
        }
        reader.finish()?;
        Ok((heads, data))
    }

    #[test]
    fn wrapped_head_matches_the_cbor_encoder() {
        for len in [0].into_iter().chain(BOUNDARY_LENGTHS) {
            let (expected, ()) = encode::emit(&BytesHeader(len));
            assert_eq!(wrapped_head(len), expected, "{len:#x}");
        }
    }

    #[test]
    fn head_value_reads_what_the_cbor_decoder_reads() {
        for len in BOUNDARY_LENGTHS {
            let head = wrapped_head(len);
            let (decoded, shortest, decoded_len): (Head, bool, usize) =
                decode::parse(&head).unwrap();
            assert_eq!(decoded.marker, Marker::Bytes(Some(len)));
            assert!(shortest, "{len:#x}");
            assert_eq!(head_len(head[0]), Some(decoded_len));
            assert_eq!(head_value(&head), len, "{len:#x}");
        }
    }

    #[test]
    fn a_direct_bundle_passes_through_uncopied() {
        let bundle = bpv7_bundle(100);
        let mut reader = StreamReader::new();
        let pushed = reader.push(bundle.clone()).unwrap();
        assert_eq!(
            pushed.head,
            Some(StreamHead {
                form: Form::Direct,
                version: BundleVersion::V7,
            })
        );
        assert_eq!(pushed.data, bundle);
        assert_eq!(pushed.data.as_ptr(), bundle.as_ptr());
        assert_eq!(reader.received(), 100);
        reader.finish().unwrap();
    }

    #[test]
    fn a_wrapped_bundle_passes_through_uncopied() {
        let bundle = bpv7_bundle(300);
        let stream = wrapped(&bundle);
        let mut reader = StreamReader::new();
        let pushed = reader.push(stream.clone()).unwrap();
        assert_eq!(
            pushed.head,
            Some(StreamHead {
                form: Form::Wrapped { len: 300 },
                version: BundleVersion::V7,
            })
        );
        assert_eq!(pushed.data, bundle);
        assert_eq!(pushed.data.as_ptr(), stream[3..].as_ptr());
        reader.finish().unwrap();
    }

    #[test]
    fn a_bpv6_bundle_is_recognized_in_both_forms() {
        let bundle = Bytes::from_static(&[BPV6_OCTET, 1, 2, 3]);
        for (stream, form) in [
            (bundle.clone(), Form::Direct),
            (wrapped(&bundle), Form::Wrapped { len: 4 }),
        ] {
            let (heads, data) = read([stream]).unwrap();
            assert_eq!(
                heads,
                [StreamHead {
                    form,
                    version: BundleVersion::V6,
                }]
            );
            assert_eq!(data, bundle);
        }
    }

    // Every way of cutting a stream in two reads the same as the whole
    // stream, for each head size a test can afford to fill.
    #[test]
    fn any_split_of_the_stream_reads_the_same() {
        for len in [1, 23, 24, 0xff, 0x100, 0x1_0000] {
            let bundle = bpv7_bundle(len);
            for stream in [bundle.clone(), wrapped(&bundle)] {
                let whole = read([stream.clone()]).unwrap();
                assert_eq!(whole.1, bundle);
                for at in (0..=stream.len()).filter(|at| *at < 16 || stream.len() - at < 16) {
                    let split = read([stream.slice(..at), stream.slice(at..)]).unwrap();
                    assert_eq!(split, whole, "len {len}, split at {at}");
                }
            }
        }
    }

    #[test]
    fn a_stream_pushed_one_octet_at_a_time_reads_the_same() {
        let bundle = bpv7_bundle(40);
        for stream in [bundle.clone(), wrapped(&bundle)] {
            let octets = (0..stream.len()).map(|i| stream.slice(i..=i));
            assert_eq!(read([stream.clone()]).unwrap(), read(octets).unwrap());
        }
    }

    // RFC 8949 permits a longer head than the shortest, and Section 4.3.2
    // does not require the shortest.
    #[test]
    fn a_longer_than_shortest_head_is_accepted() {
        let stream = Bytes::from_static(&[0x59, 0x00, 0x02, BPV7_OCTET, 0x00]);
        let (heads, data) = read([stream]).unwrap();
        assert_eq!(heads[0].form, Form::Wrapped { len: 2 });
        assert_eq!(data, [BPV7_OCTET, 0x00]);
    }

    #[test]
    fn empty_chunks_change_nothing() {
        let bundle = bpv7_bundle(30);
        let stream = wrapped(&bundle);
        let chunks = [
            Bytes::new(),
            stream.slice(..1),
            Bytes::new(),
            stream.slice(1..),
            Bytes::new(),
        ];
        assert_eq!(read(chunks).unwrap(), read([stream]).unwrap());
    }

    #[test]
    fn an_unlisted_first_octet_is_refused() {
        for octet in [
            0x00, 0x05, 0x07, 0x3f, 0x5c, 0x5d, 0x5e, 0x60, 0x9e, 0xa0, 0xff,
        ] {
            assert_eq!(
                read([Bytes::from(vec![octet, BPV7_OCTET])]),
                Err(Error::UnknownFirstOctet(octet)),
                "{octet:#04x}"
            );
        }
    }

    #[test]
    fn an_indefinite_length_byte_string_is_refused() {
        let stream = Bytes::from_static(&[INDEFINITE_BYTE_STRING, 0x41, BPV7_OCTET, 0xff]);
        assert_eq!(read([stream]), Err(Error::IndefiniteLength));
    }

    #[test]
    fn an_empty_byte_string_is_refused() {
        assert_eq!(
            read([Bytes::from_static(&[0x40])]),
            Err(Error::EmptyContent)
        );
        assert_eq!(
            read([Bytes::from_static(&[0x58, 0x00])]),
            Err(Error::EmptyContent)
        );
    }

    #[test]
    fn wrapped_content_without_a_version_octet_is_refused() {
        for octet in [0x00, 0x05, 0x07, 0x40, 0x9e] {
            assert_eq!(
                read([Bytes::from(vec![0x42, octet, 0x00])]),
                Err(Error::UnknownVersionOctet(octet)),
                "{octet:#04x}"
            );
        }
    }

    #[test]
    fn data_past_the_declared_length_is_refused() {
        let bundle = bpv7_bundle(10);
        let mut stream = wrapped(&bundle).to_vec();
        stream.push(0x00);
        assert_eq!(
            read([Bytes::from(stream.clone())]),
            Err(Error::ExcessData { declared: 10 })
        );

        // The excess arriving alone, after the declared length is complete.
        let (last, rest) = stream.split_last().unwrap();
        let chunks = [Bytes::from(rest.to_vec()), Bytes::from(vec![*last])];
        assert_eq!(read(chunks), Err(Error::ExcessData { declared: 10 }));
    }

    #[test]
    fn a_stream_ending_early_is_refused() {
        assert_eq!(read([]), Err(Error::Empty));
        assert_eq!(read([Bytes::new()]), Err(Error::Empty));

        let head = wrapped_head(0x1_0000);
        assert_eq!(read([head.slice(..1)]), Err(Error::TruncatedHead));
        assert_eq!(read([head.slice(..4)]), Err(Error::TruncatedHead));
        assert_eq!(
            read([head.clone()]),
            Err(Error::Truncated {
                declared: 0x1_0000,
                received: 0,
            })
        );
        assert_eq!(
            read([head, Bytes::from_static(&[BPV7_OCTET, 0x00])]),
            Err(Error::Truncated {
                declared: 0x1_0000,
                received: 2,
            })
        );
    }

    // The largest declared length is read without overflow and refused only
    // at FIN.
    #[test]
    fn the_largest_declared_length_is_read_and_refused_at_fin() {
        let head = wrapped_head(u64::MAX);
        let mut reader = StreamReader::new();
        assert_eq!(reader.push(head).unwrap().head, None);
        let pushed = reader.push(Bytes::from_static(&[BPV7_OCTET])).unwrap();
        assert_eq!(pushed.head.unwrap().form, Form::Wrapped { len: u64::MAX });
        assert_eq!(
            reader.finish(),
            Err(Error::Truncated {
                declared: u64::MAX,
                received: 1,
            })
        );
    }

    #[test]
    fn a_failed_reader_repeats_its_error() {
        let mut reader = StreamReader::new();
        let error = Error::UnknownFirstOctet(0x00);
        assert_eq!(reader.push(Bytes::from_static(&[0x00])), Err(error.clone()));
        assert_eq!(reader.push(bpv7_bundle(10)), Err(error.clone()));
        assert_eq!(reader.finish(), Err(error));
    }
}
