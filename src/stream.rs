use std::{io, mem};
use std::io::{BufWriter, Write};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::response::{Response, IntoResponse};
use bytes::{Bytes, BytesMut};
use http_body::{Body, SizeHint, Frame};
use futures::{FutureExt, Stream};
use pin_project::pin_project;
use tokio::io::{AsyncReadExt, BufReader, ReadBuf};

use crate::{RangeBody, ByteRange};

const IO_BUFFER_SIZE: usize = 64 * 1024;

/// Response body stream. Implements [`Stream`], [`Body`], and [`IntoResponse`].
#[pin_project]
#[derive(Debug)]
pub struct RangedStream<B> {
    state: StreamState,
    length: u64,
    #[pin]
    body: B,
}

impl<B: RangeBody + Send + 'static> RangedStream<B> {
    pub(crate) fn new(body: B, start: u64, length: u64) -> Self {
        RangedStream {
            state: StreamState::Seek { start },
            length,
            body,
        }
    }
}

#[derive(Debug)]
enum StreamState {
    Seek { start: u64 },
    Seeking { remaining: u64 },
    Reading { buffer: BytesMut, remaining: u64 },
}

impl<B: RangeBody + Send + 'static> IntoResponse for RangedStream<B> {
    fn into_response(self) -> Response {
        Response::new(axum::body::Body::new(self))
    }
}

impl<B: RangeBody> Body for RangedStream<B> {
    type Data = Bytes;
    type Error = io::Error;

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.length)
    }

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>)
        -> Poll<Option<io::Result<Frame<Bytes>>>>
    {
        self.poll_next(cx).map(|item| item.map(|result| result.map(Frame::data)))
    }
}

impl<B: RangeBody> Stream for RangedStream<B> {
    type Item = io::Result<Bytes>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<Option<io::Result<Bytes>>> {
        let mut this = self.project();

        if let StreamState::Seek { start } = *this.state {
            match this.body.as_mut().start_seek(start) {
                Err(e) => { return Poll::Ready(Some(Err(e))); }
                Ok(()) => {
                    let remaining = *this.length;
                    *this.state = StreamState::Seeking { remaining };
                }
            }
        }

        if let StreamState::Seeking { remaining } = *this.state {
            match this.body.as_mut().poll_complete(cx) {
                Poll::Pending => { return Poll::Pending; }
                Poll::Ready(Err(e)) => { return Poll::Ready(Some(Err(e))); }
                Poll::Ready(Ok(())) => {
                    let buffer = allocate_buffer();
                    *this.state = StreamState::Reading { buffer, remaining };
                }
            }
        }

        if let StreamState::Reading { buffer, remaining } = this.state {
            let uninit = buffer.spare_capacity_mut();

            // calculate max number of bytes to read in this iteration, the
            // smaller of the buffer size and the number of bytes remaining
            let nbytes = std::cmp::min(
                uninit.len(),
                usize::try_from(*remaining).unwrap_or(usize::MAX),
            );

            let mut read_buf = ReadBuf::uninit(&mut uninit[0..nbytes]);

            match this.body.as_mut().poll_read(cx, &mut read_buf) {
                Poll::Pending => { return Poll::Pending; }
                Poll::Ready(Err(e)) => { return Poll::Ready(Some(Err(e))); }
                Poll::Ready(Ok(())) => {
                    match read_buf.filled().len() {
                        0 => { return Poll::Ready(None); }
                        n => {
                            // SAFETY: poll_read has filled the buffer with `n`
                            // additional bytes. `buffer.len` should always be
                            // 0 here, but include it for rigorous correctness
                            unsafe { buffer.set_len(buffer.len() + n); }

                            // replace state buffer and take this one to return
                            let chunk = mem::replace(buffer, allocate_buffer());

                            // subtract the number of bytes we just read from
                            // state.remaining, this usize->u64 conversion is
                            // guaranteed to always succeed, because n cannot be
                            // larger than remaining due to the cmp::min above
                            *remaining -= u64::try_from(n).unwrap();

                            // return this chunk
                            return Poll::Ready(Some(Ok(chunk.freeze())));
                        }
                    }
                }
            }
        }

        unreachable!();
    }
}

/// Multipart response body stream for multiple byte ranges.
/// Implements [`Stream`], [`Body`], and [`IntoResponse`].
#[pin_project]
#[derive(Debug)]
pub struct MultipartStream<B> {
    state: MultipartState,
    ranges: Vec<ByteRange>,
    current_range_index: usize,
    total_size: u64,
    boundary: String,
    content_type: Option<String>,
    #[pin]
    body: B,
}

impl<B: RangeBody + Send + 'static> MultipartStream<B> {
    pub(crate) fn new(body: B, ranges: Vec<ByteRange>, total_size: u64, boundary: String, content_type: Option<String>) -> Self {
        MultipartStream {
            state: MultipartState::WritingBoundary { first: true },
            ranges,
            current_range_index: 0,
            total_size,
            boundary,
            content_type,
            body,
        }
    }

}

#[derive(Debug)]
enum MultipartState {
    WritingBoundary { first: bool },
    WritingHeaders,
    Seek { start: u64 },
    Seeking { remaining: u64 },
    Reading { buffer: BytesMut, remaining: u64 },
    WritingFinalBoundary,
    Finished,
}

impl<B: RangeBody + Send + 'static> IntoResponse for MultipartStream<B> {
    fn into_response(self) -> Response {
        Response::new(axum::body::Body::new(self))
    }
}

impl<B: RangeBody> Body for MultipartStream<B> {
    type Data = Bytes;
    type Error = io::Error;

    fn size_hint(&self) -> SizeHint {
        // For multipart responses, we can't easily predict the exact size
        // due to the boundary and header overhead
        SizeHint::default()
    }

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>)
                  -> Poll<Option<io::Result<Frame<Bytes>>>>
    {
        self.poll_next(cx).map(|item| item.map(|result| result.map(Frame::data)))
    }
}

impl<B: RangeBody> Stream for MultipartStream<B> {
    type Item = io::Result<Bytes>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<Option<io::Result<Bytes>>> {
        let mut this = self.project();

        loop {
            match this.state {
                MultipartState::WritingBoundary { first } => {
                    if *this.current_range_index >= this.ranges.len() {
                        *this.state = MultipartState::WritingFinalBoundary;
                        continue;
                    }

                    let boundary_line = if *first {
                        *first = false;
                        format!("--{}\r\n", this.boundary)
                    } else {
                        format!("\r\n--{}\r\n", this.boundary)
                    };

                    *this.state = MultipartState::WritingHeaders;
                    return Poll::Ready(Some(Ok(Bytes::from(boundary_line))));
                }

                MultipartState::WritingHeaders => {
                    let range = &this.ranges[*this.current_range_index];
                    let headers = format!(
                        "Content-Type: {}\r\n\
                         Content-Range: bytes {}-{}/{}\r\n\r\n",
                        this.content_type.clone().unwrap_or_else(||"application/octet-stream".to_string()),
                        range.start,
                        range.end_exclusive - 1, // HTTP ranges are inclusive
                        this.total_size
                    );

                    *this.state = MultipartState::Seek { start: range.start };
                    return Poll::Ready(Some(Ok(Bytes::from(headers))));
                }

                MultipartState::Seek { start } => {
                    match this.body.as_mut().start_seek(*start) {
                        Err(e) => return Poll::Ready(Some(Err(e))),
                        Ok(()) => {
                            let range = &this.ranges[*this.current_range_index];
                            let remaining = range.len();
                            *this.state = MultipartState::Seeking { remaining };
                        }
                    }
                }

                MultipartState::Seeking { remaining } => {
                    match this.body.as_mut().poll_complete(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Ready(Ok(())) => {
                            let buffer = allocate_buffer();
                            *this.state = MultipartState::Reading { buffer, remaining: *remaining };
                        }
                    }
                }

                MultipartState::Reading { buffer, remaining } => {
                    if *remaining == 0 {
                        *this.current_range_index += 1;
                        *this.state = MultipartState::WritingBoundary { first: false };
                        continue;
                    }

                    let uninit = buffer.spare_capacity_mut();

                    // Calculate max number of bytes to read in this iteration
                    let nbytes = std::cmp::min(
                        uninit.len(),
                        usize::try_from(*remaining).unwrap_or(usize::MAX),
                    );

                    let mut read_buf = ReadBuf::uninit(&mut uninit[0..nbytes]);

                    match this.body.as_mut().poll_read(cx, &mut read_buf) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Ready(Ok(())) => {
                            match read_buf.filled().len() {
                                0 => {
                                    // End of current range
                                    *this.current_range_index += 1;
                                    *this.state = MultipartState::WritingBoundary { first: false };
                                    continue;
                                }
                                n => {
                                    // SAFETY: poll_read has filled the buffer with `n` additional bytes
                                    unsafe { buffer.set_len(buffer.len() + n); }

                                    // Replace state buffer and take this one to return
                                    let chunk = mem::replace(buffer, allocate_buffer());

                                    // Subtract the number of bytes we just read
                                    *remaining -= u64::try_from(n).unwrap();

                                    return Poll::Ready(Some(Ok(chunk.freeze())));
                                }
                            }
                        }
                    }
                }

                MultipartState::WritingFinalBoundary => {
                    let final_boundary = format!("\r\n--{}--\r\n", this.boundary);
                    *this.state = MultipartState::Finished;
                    return Poll::Ready(Some(Ok(Bytes::from(final_boundary))));
                }

                MultipartState::Finished => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

fn allocate_buffer() -> BytesMut {
    BytesMut::with_capacity(IO_BUFFER_SIZE)
}


#[derive(Debug)]
pub struct RangePart {
    pub content_range: Option<String>,
    pub data: Bytes,
}

#[derive(Debug, Copy, Clone)]
pub enum ExtractBoundaryError {
    InvalidContentType,
}

pub fn extract_boundary(content_type: &str) -> Result<String, ExtractBoundaryError> {
    use ExtractBoundaryError::*;
    // Parse "multipart/byteranges; boundary=something"
    let boundary_prefix = "boundary=";
    content_type
        .split(';')
        .find_map(|part| {
            let trimmed = part.trim();
            if trimmed.starts_with(boundary_prefix) {
                Some(trimmed[boundary_prefix.len()..].trim_matches('"').to_string())
            } else {
                None
            }
        })
        .ok_or(InvalidContentType)
}

use multer::Multipart;

// pub async fn parse_multipart_response(
//     body: Bytes,
//     boundary: &str
// ) -> Result<Vec<(String, Bytes)>, multer::Error> {
//     use std::convert::Infallible;
// 
//     use bytes::Bytes;
//     use futures_util::stream::once;
//     use multer::Multipart;
//     
//     let stream = once(async move { Result::<Bytes, Infallible>::Ok(body) });
//     let mut multipart = Multipart::new(stream, boundary);
// 
//     let mut result = vec![];
//     while let Some(field) = multipart.next_field().await.unwrap() {
//         let content_type = field.headers().get("content-type")
//             .and_then(|v| v.to_str().ok())
//             .unwrap_or("unknown")
//             .to_string();
//         println!("Content-Type: {}", content_type);
//         let headers = field.headers().clone();
//         println!("Headers: {:?}", headers);
//         let r = field.bytes().await.unwrap();
//         result.push((content_type, r));
//         // println!("Field: {:?}", field.text().await)
//     }
//     Ok(result)
// 
//     // use tokio::io::{AsyncRead, AsyncBufRead, AsyncBufReadExt};
//     //
//     // // // use futures::stream;
//     // // let body = async_stream::IntoStream::from(async move {
//     // //     let reader = BufReader::new(body);
//     // //     let mut buf_writer = BufWriter::new(Vec::new());
//     // //     buf_writer.write_all(reader.get_ref()).await?;
//     // //     buf_writer.flush().await?;
//     // //     Ok::<_, std::io::Error>(buf_writer.into_inner())
//     // // });
//     // // let body = stream::once(async { Ok::<_, std::io::Error>(body) });
//     // let mut multipart = Multipart::new(
//     //     async_stream::stream! {
//     //         yield Ok::<_, std::io::Error>(body);
//     //     }, boundary);
//     // let mut parts = Vec::new();
//     //
//     // while let Some(mut field) = multipart.next_field().await.inspect_err(|e| eprintln!("next field error: {:?}", e))? {
//     //     let headers = field.headers().clone();
//     //     println!("Headers: {:?}", headers);
//     //     let data = field.bytes().await.inspect_err(|e| eprintln!("field bytes error: {:?}", e))?;
//     //
//     //     // Extract Content-Range header if present
//     //     let content_range = headers
//     //         .get("content-range")
//     //         .and_then(|v| v.to_str().ok())
//     //         .unwrap_or("unknown")
//     //         .to_string();
//     //
//     //     parts.push((content_range, data));
//     // }
//     //
//     // Ok(parts)
// }
