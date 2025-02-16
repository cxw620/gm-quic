use crate::varint::{be_varint, VarInt, WriteVarInt};

/// RETIRE_CONNECTION_ID frame.
///
/// ```text
/// RETIRE_CONNECTION_ID Frame {
///   Type (i) = 0x19,
///   Sequence Number (i),
/// }
/// ```
///
/// See [RETIRE_CONNECTION_ID Frames](https://www.rfc-editor.org/rfc/rfc9000.html#name-retire_connection_id-frames)
/// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html) for more details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetireConnectionIdFrame {
    pub sequence: VarInt,
}

const RETIRE_CONNECTION_ID_FRAME_TYPE: u8 = 0x19;

impl super::BeFrame for RetireConnectionIdFrame {
    fn frame_type(&self) -> super::FrameType {
        super::FrameType::RetireConnectionId
    }

    fn max_encoding_size(&self) -> usize {
        1 + 8
    }

    fn encoding_size(&self) -> usize {
        1 + self.sequence.encoding_size()
    }
}

/// Parse a RETIRE_CONNECTION_ID frame from the input buffer,
/// [nom](https://docs.rs/nom/latest/nom/) parser style.
pub fn be_retire_connection_id_frame(input: &[u8]) -> nom::IResult<&[u8], RetireConnectionIdFrame> {
    use nom::{combinator::map, Parser};
    map(be_varint, |sequence| RetireConnectionIdFrame { sequence }).parse(input)
}

impl<T: bytes::BufMut> super::io::WriteFrame<RetireConnectionIdFrame> for T {
    fn put_frame(&mut self, frame: &RetireConnectionIdFrame) {
        self.put_u8(RETIRE_CONNECTION_ID_FRAME_TYPE);
        self.put_varint(&frame.sequence);
    }
}

#[cfg(test)]
mod tests {
    use super::{be_retire_connection_id_frame, RetireConnectionIdFrame};
    use crate::{
        frame::{io::WriteFrame, BeFrame, FrameType},
        varint::VarInt,
    };

    #[test]
    fn test_retire_connection_id_frame() {
        let frame = RetireConnectionIdFrame {
            sequence: VarInt::from_u32(0x1234),
        };
        assert_eq!(frame.frame_type(), FrameType::RetireConnectionId);
        assert_eq!(frame.max_encoding_size(), 1 + 8);
        assert_eq!(frame.encoding_size(), 1 + 2);
    }

    #[test]
    fn test_read_retire_connection_id_frame() {
        let buf = vec![0x52, 0x34];
        let (remain, frame) = be_retire_connection_id_frame(&buf).unwrap();
        assert!(remain.is_empty());
        assert_eq!(
            frame,
            RetireConnectionIdFrame {
                sequence: VarInt::from_u32(0x1234),
            }
        );
    }

    #[test]
    fn test_write_retire_connection_id_frame() {
        let mut buf = Vec::new();
        let frame = RetireConnectionIdFrame {
            sequence: VarInt::from_u32(0x1234),
        };
        buf.put_frame(&frame);
        assert_eq!(buf, vec![0x19, 0x52, 0x34]);
    }
}
