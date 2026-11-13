//! The answer of Query Blob Contents: an Avro object container file whose records are the query's messages.
//!
//! Layout, copied byte for byte from the recorded answers of the real service (`testdata/*.avro`, decoded from
//! the Python SDK recordings):
//!
//! * the header: `Obj` and version 1, a metadata map with the single entry `avro.schema` (no `avro.codec`, so
//!   the codec is `null`), a count of zero to end the map, then the 16 byte sync marker;
//! * every message is a block of its own: record count 1, byte size, the record, the sync marker;
//! * a record is the index of its schema in the top-level union (0 `resultData`, 1 `error`, 2 `progress`,
//!   3 `end`) followed by its fields. Longs are zig-zag varints, `bytes` and `string` have a long length
//!   in front, a boolean is one byte.
//!
//! The sync marker is random for every response.

/// The schema, exactly as the service sends it (a JSON array of the four record types).
pub const SCHEMA: &str = include_str!("avro_schema.json");

/// Zig-zag varint, the Avro `long`.
pub fn put_long(out: &mut Vec<u8>, v: i64) {
    let mut n = ((v << 1) ^ (v >> 63)) as u64;
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_long(out, b.len() as i64);
    out.extend_from_slice(b);
}

/// One message of the answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Message<'a> {
    Data(&'a [u8]),
    Error { fatal: bool, name: &'a str, description: &'a str, position: i64 },
    Progress { bytes_scanned: i64, total_bytes: i64 },
    End { total_bytes: i64 },
}

/// Writes the container file into a buffer.
pub struct Writer {
    pub out: Vec<u8>,
    sync: [u8; 16],
    scratch: Vec<u8>,
}

impl Writer {
    /// A writer with a random sync marker.
    pub fn new() -> Writer {
        Writer::with_sync(*uuid::Uuid::new_v4().as_bytes())
    }

    /// A writer with a given sync marker (tests compare against recordings with their marker).
    pub fn with_sync(sync: [u8; 16]) -> Writer {
        let mut out = Vec::with_capacity(SCHEMA.len() + 64);
        out.extend_from_slice(b"Obj\x01");
        put_long(&mut out, 1);
        put_bytes(&mut out, b"avro.schema");
        put_bytes(&mut out, SCHEMA.as_bytes());
        put_long(&mut out, 0);
        out.extend_from_slice(&sync);
        Writer { out, sync, scratch: Vec::new() }
    }

    /// Bytes written so far (the header and all blocks).
    pub fn len(&self) -> usize {
        self.out.len()
    }

    pub fn write(&mut self, m: &Message) {
        let s = &mut self.scratch;
        s.clear();
        match m {
            Message::Data(d) => {
                put_long(s, 0);
                put_bytes(s, d);
            }
            Message::Error { fatal, name, description, position } => {
                put_long(s, 1);
                s.push(u8::from(*fatal));
                put_bytes(s, name.as_bytes());
                put_bytes(s, description.as_bytes());
                put_long(s, *position);
            }
            Message::Progress { bytes_scanned, total_bytes } => {
                put_long(s, 2);
                put_long(s, *bytes_scanned);
                put_long(s, *total_bytes);
            }
            Message::End { total_bytes } => {
                put_long(s, 3);
                put_long(s, *total_bytes);
            }
        }
        put_long(&mut self.out, 1);
        put_long(&mut self.out, self.scratch.len() as i64);
        self.out.extend_from_slice(&self.scratch);
        self.out.extend_from_slice(&self.sync);
    }

    #[cfg(test)]
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}

#[cfg(test)]
pub mod tests {
    //! A reader for the container file, written from the Avro specification (not from the writer above),
    //! and the comparison of the writer with what the real service sent.

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Msg {
        Data(Vec<u8>),
        Error { fatal: bool, name: String, description: String, position: i64 },
        Progress(i64, i64),
        End(i64),
    }

    fn get_long(b: &[u8], p: &mut usize) -> i64 {
        let (mut n, mut shift) = (0u64, 0);
        loop {
            let c = b[*p];
            *p += 1;
            n |= u64::from(c & 0x7f) << shift;
            shift += 7;
            if c & 0x80 == 0 {
                break;
            }
        }
        ((n >> 1) as i64) ^ -((n & 1) as i64)
    }

    fn get_bytes<'a>(b: &'a [u8], p: &mut usize) -> &'a [u8] {
        let n = get_long(b, p) as usize;
        let r = &b[*p..*p + n];
        *p += n;
        r
    }

    /// Returns (schema, sync marker, messages). Panics when the layout is not the one the SDKs read.
    pub fn read(b: &[u8]) -> (String, [u8; 16], Vec<Msg>) {
        assert_eq!(&b[..4], b"Obj\x01");
        let mut p = 4;
        let mut schema = String::new();
        loop {
            let mut n = get_long(b, &mut p);
            if n == 0 {
                break;
            }
            if n < 0 {
                get_long(b, &mut p);
                n = -n;
            }
            for _ in 0..n {
                let k = get_bytes(b, &mut p).to_vec();
                let v = get_bytes(b, &mut p).to_vec();
                if k == b"avro.schema" {
                    schema = String::from_utf8(v).unwrap();
                } else {
                    panic!("unexpected metadata {}", String::from_utf8_lossy(&k));
                }
            }
        }
        let sync: [u8; 16] = b[p..p + 16].try_into().unwrap();
        p += 16;
        let mut msgs = vec![];
        while p < b.len() {
            assert_eq!(get_long(b, &mut p), 1, "one record per block");
            let size = get_long(b, &mut p) as usize;
            let block = &b[p..p + size];
            p += size;
            assert_eq!(&b[p..p + 16], &sync, "sync marker after every block");
            p += 16;
            let mut q = 0;
            let idx = get_long(block, &mut q);
            msgs.push(match idx {
                0 => Msg::Data(get_bytes(block, &mut q).to_vec()),
                1 => {
                    let fatal = block[q] != 0;
                    q += 1;
                    let name = String::from_utf8(get_bytes(block, &mut q).to_vec()).unwrap();
                    let description = String::from_utf8(get_bytes(block, &mut q).to_vec()).unwrap();
                    Msg::Error { fatal, name, description, position: get_long(block, &mut q) }
                }
                2 => {
                    let a = get_long(block, &mut q);
                    Msg::Progress(a, get_long(block, &mut q))
                }
                3 => Msg::End(get_long(block, &mut q)),
                i => panic!("unknown union index {i}"),
            });
            assert_eq!(q, block.len(), "the record fills its block");
        }
        (schema, sync, msgs)
    }

    fn rebuild(file: &[u8]) -> Vec<u8> {
        let (schema, sync, msgs) = read(file);
        assert_eq!(schema, SCHEMA, "the schema of the service is ours, character for character");
        let mut w = Writer::with_sync(sync);
        for m in &msgs {
            match m {
                Msg::Data(d) => w.write(&Message::Data(d)),
                Msg::Error { fatal, name, description, position } => {
                    w.write(&Message::Error { fatal: *fatal, name, description, position: *position })
                }
                Msg::Progress(a, t) => w.write(&Message::Progress { bytes_scanned: *a, total_bytes: *t }),
                Msg::End(t) => w.write(&Message::End { total_bytes: *t }),
            }
        }
        w.finish()
    }

    #[test]
    fn the_writer_reproduces_every_recorded_answer_byte_for_byte() {
        for (name, file) in [
            ("readall", &include_bytes!("testdata/readall.avro")[..]),
            ("fatal_json", &include_bytes!("testdata/fatal_json.avro")[..]),
            ("nonfatal", &include_bytes!("testdata/nonfatal.avro")[..]),
            ("json_out", &include_bytes!("testdata/json_out.avro")[..]),
            ("headers_out", &include_bytes!("testdata/headers_out.avro")[..]),
            ("custom_out", &include_bytes!("testdata/custom_out.avro")[..]),
        ] {
            assert!(rebuild(file) == file, "{name}: the rebuilt file differs from the recorded one");
        }
    }

    #[test]
    fn the_recorded_messages_are_what_the_notes_say() {
        let (_, _, m) = read(include_bytes!("testdata/fatal_json.avro"));
        assert_eq!(m[0], Msg::Data(vec![]));
        assert_eq!(
            m[1],
            Msg::Error {
                fatal: true,
                name: "ParseError".into(),
                description: "Unexpected token 'n' at [byte: 1]. Expecting token '\"'. ".into(),
                position: 0
            }
        );
        assert_eq!(m[2], Msg::Progress(414, 414));
        assert_eq!(m[3], Msg::End(414));
        let (_, _, m) = read(include_bytes!("testdata/nonfatal.avro"));
        assert!(matches!(&m[1], Msg::Error { fatal: false, name, position: 118, .. } if name == "InvalidColumnOrdinal"));
    }

    #[test]
    fn longs_are_zigzag_varints() {
        let mut v = vec![];
        for (n, want) in [(0i64, &[0u8][..]), (-1, &[1]), (1, &[2]), (-64, &[0x7f]), (64, &[0x80, 1]), (2256, &[0xa0, 0x23])] {
            v.clear();
            put_long(&mut v, n);
            assert_eq!(v, want, "{n}");
        }
        v.clear();
        put_long(&mut v, i64::MIN);
        assert_eq!(v.len(), 10);
    }
}
