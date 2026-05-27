//! Data-plane wire codec for the interactive attach transport.
//!
//! Length-prefixed binary frames: `[type:u8][len:u32 BE][payload]`. Binary
//! (not the `Event` NDJSON control plane) because full-screen repaints are
//! high-volume and base64-in-JSON would hurt — the same data/control split
//! orca uses. See `docs/attach-transport.md` for the frame table.

use std::io::{self, Read, Write};

// Frame type tags. Stable across a PROTO_VERSION; add new tags at the end.
const T_HELLO: u8 = 1;
const T_SNAPSHOT: u8 = 2;
const T_DATA: u8 = 3;
const T_INPUT: u8 = 4;
const T_RESIZE: u8 = 5;
const T_SIGNAL: u8 = 6;
const T_DATA_ACK: u8 = 7;
const T_EXIT: u8 = 8;

/// One frame on the data-plane pipe. Direction is by convention (see the
/// table in the design doc), not enforced by the type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // variants are produced/consumed as backends land (phases 2–5)
pub(crate) enum Frame {
    /// C→H, first frame on attach: the client's initial terminal size.
    Hello { cols: u16, rows: u16 },
    /// H→C, exactly once after Hello: ANSI bytes that repaint current state.
    Snapshot(Vec<u8>),
    /// H→C: live PTY output.
    Data(Vec<u8>),
    /// C→H: raw keystrokes.
    Input(Vec<u8>),
    /// C→H: terminal resized (SIGWINCH).
    Resize { cols: u16, rows: u16 },
    /// C→H: a named signal (e.g. "detach", "INT", "TERM").
    Signal(String),
    /// C→H: cumulative bytes the client has rendered, for flow control.
    DataAck(u64),
    /// H→C: the agent/PTY exited with this code.
    Exit(i32),
    /// A frame whose tag this build doesn't know — kept so a newer peer's
    /// additions don't break decode. Consumers ignore it.
    Unknown { tag: u8, payload: Vec<u8> },
}

#[allow(dead_code)] // exercised by tests now; by the pump in phase 2
impl Frame {
    /// Serialize and write the frame, then flush.
    pub(crate) fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let (tag, payload) = self.to_wire();
        let mut hdr = [0u8; 5];
        hdr[0] = tag;
        hdr[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        w.write_all(&hdr)?;
        w.write_all(&payload)?;
        w.flush()
    }

    /// Read one frame. `Ok(None)` is a clean EOF (peer closed between
    /// frames). A truncated frame or malformed fixed-width payload is an
    /// `InvalidData` error.
    pub(crate) fn decode(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut hdr = [0u8; 5];
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload)?;
        Ok(Some(Frame::from_wire(hdr[0], payload)?))
    }

    fn to_wire(&self) -> (u8, Vec<u8>) {
        match self {
            Frame::Hello { cols, rows } => (T_HELLO, dims(*cols, *rows)),
            Frame::Snapshot(b) => (T_SNAPSHOT, b.clone()),
            Frame::Data(b) => (T_DATA, b.clone()),
            Frame::Input(b) => (T_INPUT, b.clone()),
            Frame::Resize { cols, rows } => (T_RESIZE, dims(*cols, *rows)),
            Frame::Signal(s) => (T_SIGNAL, s.clone().into_bytes()),
            Frame::DataAck(n) => (T_DATA_ACK, n.to_be_bytes().to_vec()),
            Frame::Exit(code) => (T_EXIT, code.to_be_bytes().to_vec()),
            Frame::Unknown { tag, payload } => (*tag, payload.clone()),
        }
    }

    fn from_wire(tag: u8, payload: Vec<u8>) -> io::Result<Frame> {
        let frame = match tag {
            T_HELLO => {
                let (cols, rows) = parse_dims(&payload)?;
                Frame::Hello { cols, rows }
            }
            T_SNAPSHOT => Frame::Snapshot(payload),
            T_DATA => Frame::Data(payload),
            T_INPUT => Frame::Input(payload),
            T_RESIZE => {
                let (cols, rows) = parse_dims(&payload)?;
                Frame::Resize { cols, rows }
            }
            T_SIGNAL => Frame::Signal(
                String::from_utf8(payload).map_err(|_| invalid("Signal payload is not UTF-8"))?,
            ),
            T_DATA_ACK => Frame::DataAck(u64::from_be_bytes(
                payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("DataAck payload must be 8 bytes"))?,
            )),
            T_EXIT => Frame::Exit(i32::from_be_bytes(
                payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("Exit payload must be 4 bytes"))?,
            )),
            other => Frame::Unknown {
                tag: other,
                payload,
            },
        };
        Ok(frame)
    }
}

fn dims(cols: u16, rows: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(4);
    v.extend_from_slice(&cols.to_be_bytes());
    v.extend_from_slice(&rows.to_be_bytes());
    v
}

fn parse_dims(payload: &[u8]) -> io::Result<(u16, u16)> {
    if payload.len() != 4 {
        return Err(invalid("dimensions payload must be 4 bytes"));
    }
    Ok((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(f: Frame) {
        let mut buf = Vec::new();
        f.encode(&mut buf).unwrap();
        let decoded = Frame::decode(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(f, decoded, "frame did not survive encode/decode");
    }

    #[test]
    fn every_variant_round_trips() {
        round_trip(Frame::Hello {
            cols: 120,
            rows: 40,
        });
        round_trip(Frame::Snapshot(b"\x1b[?1049h\x1b[2Jhi".to_vec()));
        round_trip(Frame::Data(vec![0, 1, 2, 255, 254]));
        round_trip(Frame::Input(b"ls -la\r".to_vec()));
        round_trip(Frame::Resize { cols: 80, rows: 24 });
        round_trip(Frame::Signal("detach".into()));
        round_trip(Frame::DataAck(1 << 40));
        round_trip(Frame::Exit(-1));
    }

    #[test]
    fn back_to_back_frames_decode_in_order() {
        let mut buf = Vec::new();
        Frame::Hello { cols: 80, rows: 24 }
            .encode(&mut buf)
            .unwrap();
        Frame::Data(b"abc".to_vec()).encode(&mut buf).unwrap();
        Frame::Exit(0).encode(&mut buf).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(
            Frame::decode(&mut r).unwrap().unwrap(),
            Frame::Hello { cols: 80, rows: 24 }
        );
        assert_eq!(
            Frame::decode(&mut r).unwrap().unwrap(),
            Frame::Data(b"abc".to_vec())
        );
        assert_eq!(Frame::decode(&mut r).unwrap().unwrap(), Frame::Exit(0));
        assert_eq!(
            Frame::decode(&mut r).unwrap(),
            None,
            "clean EOF after last frame"
        );
    }

    #[test]
    fn unknown_tag_is_preserved_not_fatal() {
        // hand-roll a frame with a future tag (200) and a payload
        let mut buf = vec![200u8];
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(b"xyz");
        let decoded = Frame::decode(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(
            decoded,
            Frame::Unknown {
                tag: 200,
                payload: b"xyz".to_vec()
            }
        );
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let mut buf = vec![T_DATA];
        buf.extend_from_slice(&10u32.to_be_bytes()); // claims 10 bytes
        buf.extend_from_slice(b"only4"); // but supplies 5
        assert!(Frame::decode(&mut buf.as_slice()).is_err());
    }
}
