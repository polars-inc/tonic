use super::{
    compression::{compress, CompressionEncoding, SingleMessageCompressionOverride,CompressionSettings},
    Encoder, SliceBuffer, DEFAULT_MAX_SEND_MESSAGE_SIZE, HEADER_SIZE,BufferSettings
};
use crate::{codec::EncodeBuf, Code, Status};
use bytes::{Buf, BufMut};
use http::HeaderMap;
use http_body::Body;
use pin_project::pin_project;
use std::io::Seek;
use std::{
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};
use tokio_stream::{Stream, StreamExt};

const SLICE_SIZE: usize = 128;
const YIELD_THRESHOLD: usize = 32 * 1024;

pub(crate) fn encode_server<T, U>(
    encoder: T,
    source: U,
    compression_encoding: Option<CompressionEncoding>,
    compression_override: SingleMessageCompressionOverride,
    max_message_size: Option<usize>,
) -> EncodeBody<impl Stream<Item = Result<SliceBuffer, Status>>>
where
    T: Encoder<Error = Status>,
    U: Stream<Item = Result<T::Item, Status>>,
{
    let stream = EncodedBytes::new(
        encoder,
        source.fuse(),
        compression_encoding,
        compression_override,
        max_message_size,
    );

    EncodeBody::new_server(stream)
}

pub(crate) fn encode_client<T, U>(
    encoder: T,
    source: U,
    compression_encoding: Option<CompressionEncoding>,
    max_message_size: Option<usize>,
) -> EncodeBody<impl Stream<Item = Result<SliceBuffer, Status>>>
where
    T: Encoder<Error = Status>,
    U: Stream<Item = T::Item>,
{
    let stream = EncodedBytes::new(
        encoder,
        source.fuse().map(Ok),
        compression_encoding,
        SingleMessageCompressionOverride::default(),
        max_message_size,
    );
    EncodeBody::new_client(stream)
}

/// Combinator for efficient encoding of messages into reasonably sized buffers.
/// EncodedBytes encodes ready messages from its delegate stream into a SliceBuffer,
/// splitting off and yielding a buffer when either:
///  * The delegate stream polls as not ready, or
///  * The encoded buffer surpasses YIELD_THRESHOLD.
#[pin_project(project = EncodedBytesProj)]
#[derive(Debug)]
pub(crate) struct EncodedBytes<T, U>
where
    T: Encoder<Error = Status>,
    U: Stream<Item = Result<T::Item, Status>>,
{
    #[pin]
    source: U,
    encoder: T,
    compression_encoding: Option<CompressionEncoding>,
    max_message_size: Option<usize>,
    buf: SliceBuffer,
    uncompression_buf: SliceBuffer,
}

impl<T, U> EncodedBytes<T, U>
where
    T: Encoder<Error = Status>,
    U: Stream<Item = Result<T::Item, Status>>,
{
    // `source` should be fused stream.
    fn new(
        encoder: T,
        source: U,
        compression_encoding: Option<CompressionEncoding>,
        compression_override: SingleMessageCompressionOverride,
        max_message_size: Option<usize>,
    ) -> Self {
        dbg!("HERRE");
        let buffer_settings = encoder.buffer_settings();
        let buf = SliceBuffer::with_capacity(SLICE_SIZE, buffer_settings.buffer_size);


        let compression_encoding =
            if compression_override == SingleMessageCompressionOverride::Disable {
                None
            } else {
                compression_encoding
            };

        let uncompression_buf = if compression_encoding.is_some() {
            SliceBuffer::with_capacity(SLICE_SIZE, buffer_settings.buffer_size)
        } else {
            SliceBuffer::default()
        };

        Self {
            source,
            encoder,
            compression_encoding,
            max_message_size,
            buf,
            uncompression_buf,
        }
    }
}

impl<T, U> Stream for EncodedBytes<T, U>
where
    T: Encoder<Error = Status>,
    U: Stream<Item = Result<T::Item, Status>>,
{
    type Item = Result<SliceBuffer, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let EncodedBytesProj {
            mut source,
            encoder,
            compression_encoding,
            max_message_size,
            buf,
            uncompression_buf,
        } = self.project();
        let buffer_settings = encoder.buffer_settings();

        loop {
            match source.as_mut().poll_next(cx) {
                Poll::Pending if buf.is_empty() => {
                    return Poll::Pending;
                }
                Poll::Ready(None) if buf.is_empty() => {
                    return Poll::Ready(None);
                }
                Poll::Pending | Poll::Ready(None) => {
                    return Poll::Ready(Some(Ok(buf.split_to(buf.remaining()))));
                }
                Poll::Ready(Some(Ok(item))) => {
                    if let Err(status) = encode_item(
                        encoder,
                        buf,
                        uncompression_buf,
                        *compression_encoding,
                        *max_message_size,
                        buffer_settings,
                        item,
                    ) {
                        return Poll::Ready(Some(Err(status)));
                    }

                    if buf.len() >= buffer_settings.yield_threshold {
                        return Poll::Ready(Some(Ok(buf.split_to(buf.remaining()))));
                    }
                }
                Poll::Ready(Some(Err(status))) => {
                    return Poll::Ready(Some(Err(status)));
                }
            }
        }
    }
}

fn encode_item<T>(
    encoder: &mut T,
    buf: &mut SliceBuffer,
    uncompression_buf: &mut SliceBuffer,
    compression_encoding: Option<CompressionEncoding>,
    max_message_size: Option<usize>,
    buffer_settings: BufferSettings,
    item: T::Item,
) -> Result<(), Status>
where
    T: Encoder<Error = Status>,
{
    let offset = buf.len();

    buf.reserve(HEADER_SIZE);
    unsafe {
        buf.advance_mut(HEADER_SIZE);
    }

    if let Some(encoding) = compression_encoding {
        uncompression_buf.clear();

        encoder
            .encode(item, &mut EncodeBuf::new(uncompression_buf))
            .map_err(|err| Status::internal(format!("Error encoding: {}", err)))?;

        let uncompressed_len = uncompression_buf.len();

        compress(
            CompressionSettings {
                encoding,
                buffer_growth_interval: buffer_settings.buffer_size,
            },
            uncompression_buf,
            buf,
            uncompressed_len,
        )
        .map_err(|err| Status::internal(format!("Error compressing: {}", err)))?;

    } else {
        encoder
            .encode(item, &mut EncodeBuf::new(buf))
            .map_err(|err| Status::internal(format!("Error encoding: {}", err)))?;
    }

    // now that we know length, we can write the header
    let encoded_len = buf.len() - offset;
    let mut cursor = buf.cursor();
    cursor
        .seek(io::SeekFrom::End(-1 * encoded_len as i64))
        .map_err(|err| Status::internal(format!("Error encoding: {}", err)))?;
    finish_encoding(
        compression_encoding,
        max_message_size,
        encoded_len,
        &mut cursor,
    )
}

fn finish_encoding(
    compression_encoding: Option<CompressionEncoding>,
    max_message_size: Option<usize>,
    encoded_len: usize,
    buf: &mut dyn io::Write,
) -> Result<(), Status> {
    let len = encoded_len - HEADER_SIZE;
    let limit = max_message_size.unwrap_or(DEFAULT_MAX_SEND_MESSAGE_SIZE);
    if len > limit {
        return Err(Status::new(
            Code::OutOfRange,
            format!(
                "Error, message length too large: found {} bytes, the limit is: {} bytes",
                len, limit
            ),
        ));
    }

    if len > std::u32::MAX as usize {
        return Err(Status::resource_exhausted(format!(
            "Cannot return body with more than 4GB of data but got {len} bytes"
        )));
    }
    buf.write(&[compression_encoding.is_some() as u8])
        .map_err(|err| Status::internal(format!("Error encoding: {}", err)))?;
    buf.write(&(len as u32).to_be_bytes())
        .map_err(|err| Status::internal(format!("Error encoding: {}", err)))?;

    Ok(())
}

#[derive(Debug)]
enum Role {
    Client,
    Server,
}

#[pin_project]
#[derive(Debug)]
pub(crate) struct EncodeBody<S> {
    #[pin]
    inner: S,
    state: EncodeState,
}

#[derive(Debug)]
struct EncodeState {
    error: Option<Status>,
    role: Role,
    is_end_stream: bool,
}

impl<S> EncodeBody<S>
where
    S: Stream<Item = Result<SliceBuffer, Status>>,
{
    pub(crate) fn new_client(inner: S) -> Self {
        Self {
            inner,
            state: EncodeState {
                error: None,
                role: Role::Client,
                is_end_stream: false,
            },
        }
    }

    pub(crate) fn new_server(inner: S) -> Self {
        Self {
            inner,
            state: EncodeState {
                error: None,
                role: Role::Server,
                is_end_stream: false,
            },
        }
    }
}

impl EncodeState {
    fn trailers(&mut self) -> Result<Option<HeaderMap>, Status> {
        match self.role {
            Role::Client => Ok(None),
            Role::Server => {
                if self.is_end_stream {
                    return Ok(None);
                }

                let status = if let Some(status) = self.error.take() {
                    self.is_end_stream = true;
                    status
                } else {
                    Status::new(Code::Ok, "")
                };

                Ok(Some(status.to_header_map()?))
            }
        }
    }
}

impl<S> Body for EncodeBody<S>
where
    S: Stream<Item = Result<SliceBuffer, Status>>,
{
    type Data = SliceBuffer;
    type Error = Status;

    fn is_end_stream(&self) -> bool {
        self.state.is_end_stream
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let self_proj = self.project();
        match ready!(self_proj.inner.poll_next(cx)) {
            Some(Ok(d)) => Some(Ok(d)).into(),
            Some(Err(status)) => match self_proj.state.role {
                Role::Client => Some(Err(status)).into(),
                Role::Server => {
                    self_proj.state.error = Some(status);
                    None.into()
                }
            },
            None => None.into(),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Status>> {
        Poll::Ready(self.project().state.trailers())
    }
}
