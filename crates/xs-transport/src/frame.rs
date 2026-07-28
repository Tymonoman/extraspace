//! Reading and writing extraspace frames over a stream.
//!
//! Split into independent halves so the read and write directions can live on
//! separate tasks without a lock between them -- video frames go out while touch
//! events and stats come back, and neither should ever wait on the other.

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use xs_proto::{Channel, Header, ProtoError, HEADER_LEN};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),

    #[error("peer closed the connection")]
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;

/// A frame as it came off the wire.
#[derive(Debug, Clone)]
pub struct Frame {
    pub header: Header,
    pub payload: Bytes,
}

impl Frame {
    pub fn channel(&self) -> Channel {
        self.header.channel
    }
}

pub struct FrameReader<R> {
    inner: R,
    buf: BytesMut,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(64 * 1024),
        }
    }

    /// Reads exactly one frame, waiting as long as necessary.
    ///
    /// A desynced stream surfaces as [`ProtoError::BadMagic`] on the very next
    /// read rather than being silently misinterpreted, because the header carries
    /// a magic number and the length is validated against a ceiling.
    pub async fn read_frame(&mut self) -> Result<Frame> {
        let mut head = [0u8; HEADER_LEN];
        match self.inner.read_exact(&mut head).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
            Err(e) => return Err(e.into()),
        }
        let header = Header::decode(&head)?;

        self.buf.clear();
        self.buf.resize(header.len as usize, 0);
        if header.len > 0 {
            match self.inner.read_exact(&mut self.buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(Error::Closed)
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(Frame {
            header,
            payload: self.buf.split().freeze(),
        })
    }
}

pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub async fn write_frame(
        &mut self,
        channel: Channel,
        kind: u8,
        flags: u16,
        pts_us: u64,
        payload: &[u8],
    ) -> Result<()> {
        let header = Header {
            channel,
            kind,
            flags,
            len: payload.len() as u32,
            pts_us,
        };
        // One write per frame where possible: two syscalls per video frame at
        // 60fps is noise, but interleaving with another task's write is not.
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(payload);
        self.inner.write_all(&out).await?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xs_proto::{flags, TouchAction, TouchEvent};

    #[tokio::test]
    async fn frame_survives_a_roundtrip() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut writer = FrameWriter::new(client);
        let mut reader = FrameReader::new(server);

        let payload = b"hello extraspace";
        writer
            .write_frame(Channel::Control, 3, flags::KEYFRAME, 42, payload)
            .await
            .unwrap();

        let frame = reader.read_frame().await.unwrap();
        assert_eq!(frame.header.channel, Channel::Control);
        assert_eq!(frame.header.kind, 3);
        assert_eq!(frame.header.flags, flags::KEYFRAME);
        assert_eq!(frame.header.pts_us, 42);
        assert_eq!(&frame.payload[..], payload);
    }

    #[tokio::test]
    async fn back_to_back_frames_do_not_bleed_into_each_other() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut writer = FrameWriter::new(client);
        let mut reader = FrameReader::new(server);

        for i in 0..50u8 {
            let touch = TouchEvent {
                action: TouchAction::Motion,
                slot: i as u32,
                x: i as f64 * 1.5,
                y: i as f64 * 2.5,
            };
            writer
                .write_frame(Channel::Touch, 0, 0, i as u64, &touch.encode())
                .await
                .unwrap();
        }

        for i in 0..50u8 {
            let frame = reader.read_frame().await.unwrap();
            let touch = TouchEvent::decode(&frame.payload).unwrap();
            assert_eq!(touch.slot, i as u32);
            assert_eq!(touch.x, i as f64 * 1.5);
            assert_eq!(frame.header.pts_us, i as u64);
        }
    }

    #[tokio::test]
    async fn empty_payload_is_valid() {
        let (client, server) = tokio::io::duplex(1024);
        let mut writer = FrameWriter::new(client);
        let mut reader = FrameReader::new(server);

        writer.write_frame(Channel::Control, 4, 0, 7, &[]).await.unwrap();
        let frame = reader.read_frame().await.unwrap();
        assert!(frame.payload.is_empty());
        assert_eq!(frame.header.kind, 4);
    }

    #[tokio::test]
    async fn closed_peer_reports_closed_not_a_parse_error() {
        let (client, server) = tokio::io::duplex(1024);
        drop(client);
        let mut reader = FrameReader::new(server);
        assert!(matches!(reader.read_frame().await, Err(Error::Closed)));
    }

    #[tokio::test]
    async fn garbage_on_the_wire_is_rejected_by_the_magic_check() {
        let (mut client, server) = tokio::io::duplex(1024);
        tokio::io::AsyncWriteExt::write_all(&mut client, &[0xffu8; HEADER_LEN])
            .await
            .unwrap();
        let mut reader = FrameReader::new(server);
        assert!(matches!(
            reader.read_frame().await,
            Err(Error::Proto(ProtoError::BadMagic(_)))
        ));
    }
}
