// Identical wire format to raft-server's codec.
// In a larger codebase this would live in a shared `raft-wire` crate.
use std::io;

use bytes::{Buf, BufMut, BytesMut};
use raft_core::message::RaftMessage;
use tokio_util::codec::{Decoder, Encoder};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub struct RaftCodec;

impl Encoder<RaftMessage> for RaftCodec {
    type Error = io::Error;

    fn encode(&mut self, msg: RaftMessage, dst: &mut BytesMut) -> io::Result<()> {
        let payload =
            bincode::serialize(&msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let len = payload.len();
        if len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        dst.reserve(4 + len);
        dst.put_u32(len as u32);
        dst.put_slice(&payload);
        Ok(())
    }
}

impl Decoder for RaftCodec {
    type Item = RaftMessage;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<RaftMessage>> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        if src.len() < 4 + len {
            src.reserve(4 + len - src.len());
            return Ok(None);
        }
        src.advance(4);
        let payload = src.split_to(len);
        let msg = bincode::deserialize(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(msg))
    }
}
