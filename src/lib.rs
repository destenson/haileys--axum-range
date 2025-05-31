//! # axum-range
//!
//! HTTP range responses for [`axum`][1].
//!
//! Fully generic, supports any body implementing the [`RangeBody`] trait.
//!
//! Any type implementing both [`AsyncRead`] and [`AsyncSeekStart`] can be
//! used the [`KnownSize`] adapter struct. There is also special cased support
//! for [`tokio::fs::File`], see the [`KnownSize::file`] method.
//!
//! [`AsyncSeekStart`] is a trait defined by this crate which only allows
//! seeking from the start of a file. It is automatically implemented for any
//! type implementing [`AsyncSeek`].
//!
//! ```
//! use axum::Router;
//! use axum::routing::get;
//! use axum_extra::{TypedHeader, headers::Range};
//!
//! use tokio::fs::File;
//!
//! use axum_range::Ranged;
//! use axum_range::KnownSize;
//!
//! async fn file(range: Option<TypedHeader<Range>>) -> Ranged<KnownSize<File>> {
//!     let file = File::open("The Sims 1 - The Complete Collection.rar").await.unwrap();
//!     let body = KnownSize::file(file).await.unwrap();
//!     let range = range.map(|TypedHeader(range)| range);
//!     Ranged::new(range, body)
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     // build our application with a single route
//!     let _app = Router::<()>::new().route("/", get(file));
//!
//!     // run it with hyper on localhost:3000
//!     #[cfg(feature = "run_server_in_example")]
//!     axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
//!        .serve(_app.into_make_service())
//!        .await
//!        .unwrap();
//! }
//! ```
//!
//! [1]: https://docs.rs/axum

mod file;
mod stream;

use std::io;
use std::ops::Bound;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;
use axum_extra::headers::{ContentRange, ContentLength, AcceptRanges, HeaderValue, HeaderMap, Header, Range};
use tokio::io::{AsyncRead, AsyncSeek};

pub use file::KnownSize;
pub use stream::{RangedStream, MultipartStream};

/// [`AsyncSeek`] narrowed to only allow seeking from start.
pub trait AsyncSeekStart {
    /// Same semantics as [`AsyncSeek::start_seek`], always passing position as the `SeekFrom::Start` variant.
    fn start_seek(self: Pin<&mut Self>, position: u64) -> io::Result<()>;

    /// Same semantics as [`AsyncSeek::poll_complete`], returning `()` instead of the new stream position.
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

impl<T: AsyncSeek> AsyncSeekStart for T {
    fn start_seek(self: Pin<&mut Self>, position: u64) -> io::Result<()> {
        AsyncSeek::start_seek(self, io::SeekFrom::Start(position))
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncSeek::poll_complete(self, cx).map_ok(|_| ())
    }
}

/// An [`AsyncRead`] and [`AsyncSeekStart`] with a fixed known byte size.
pub trait RangeBody: AsyncRead + AsyncSeekStart {
    /// The total size of the underlying file.
    ///
    /// This should not change for the lifetime of the object once queried.
    /// Behaviour is not guaranteed if it does change.
    fn byte_size(&self) -> u64;
    
    /// Returns the block size of the body, if applicable.
    /// This is used to determine how much data to read for a range request.
    /// If not specified, the entire byte size is used.
    fn block_len(&self) -> u64 { self.byte_size() }
}

/// Represents a single byte range with start and end positions (exclusive end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end_exclusive: u64,
}

impl ByteRange {
    /// Create a new byte range with inclusive start and exclusive end.
    pub fn new(start: u64, end_exclusive: u64) -> Self {
        ByteRange { start, end_exclusive }
    }

    pub fn len(&self) -> u64 {
        self.end_exclusive - self.start
    }
}

#[derive(Debug, Clone, PartialEq)]
/// The main responder type. Implements [`IntoResponse`].
pub struct Ranged<B: RangeBody + Send + 'static> {
    range: Option<Range>,
    body: B,
}

impl<B: RangeBody + Send + 'static> Ranged<B> {
    /// Construct a ranged response over any type implementing [`RangeBody`]
    /// and an optional [`Range`] header.
    pub fn new(range: Option<Range>, body: B) -> Self {
        Ranged { range, body }
    }

    /// Responds to the request, returning headers and body as
    /// [`RangedResponse`]. Returns [`RangeNotSatisfiable`] error if requested
    /// range in header was not satisfiable.
    pub fn try_respond(self) -> Result<RangedResponse<B>, RangeNotSatisfiable> {
        let total_bytes = self.body.byte_size();

        let block_len = self.body.block_len();
        
        match self.range {
            None => {
                if total_bytes <= block_len {
                    eprintln!("No range header, returning full file of size: {}", total_bytes);
                    // no range header, return the whole file
                    let content_length = ContentLength(total_bytes);
                    let stream = RangedStream::new(self.body, 0, total_bytes);
                    Ok(RangedResponse::Full {
                        content_length,
                        stream,
                    })
                } else {
                    eprintln!("No range header, returning first {} bytes of file of size: {}", block_len, total_bytes);
                    // block_len is less than total_bytes, return the first block_len bytes
                    let content_length = ContentLength(block_len);
                    let stream = RangedStream::new(self.body, 0, block_len);
                    let content_range = ContentRange::bytes(0..block_len, total_bytes)
                        .expect("ContentRange::bytes cannot panic in this usage");
                    Ok(RangedResponse::Single {
                        content_range,
                        content_length,
                        stream,
                    })
                }
            }
            Some(range) => {
                println!("range: {:?}", range);
                // hacky way to get the range string for parsing
                let mut range_str = format!("{:?}", range).replace("Range(\"", "").replace("\")", "");
                println!("total_bytes: {}", total_bytes);
                // Parse all satisfiable ranges
                let satisfiable_ranges: Vec<_> = range
                    .satisfiable_ranges(total_bytes)
                    .map(|(start_bound, end_bound)| {
                        println!("start_bound: {:?}, end_bound: {:?}", start_bound, end_bound);
                        let start = match start_bound {
                            Bound::Included(start) => start,
                            _ => 0,
                        };

                        // Check if start position is valid
                        if start >= total_bytes {
                            eprintln!("Start position {} exceeds total bytes {}", start, total_bytes);
                            return None;
                        }

                        let end = match end_bound {
                            // HTTP byte ranges are inclusive, so we translate to exclusive by adding 1
                            Bound::Included(end) => end + 1,
                            Bound::Excluded(end) => end,
                            _ => start + block_len,
                        };
                        // Check for invalid conditions
                        if start >= end {
                            eprintln!("Start position {} is greater than or equal to end position {}", start, end);
                            return None;
                        }

                        // If end exceeds total_bytes, adjust it to total_bytes as per RFC
                        let end = {
                            let mut end = std::cmp::min(match end_bound {
                                // HTTP byte ranges are inclusive, so we translate to exclusive by adding 1
                                Bound::Included(end) => end + 1,
                                Bound::Excluded(end) => end,
                                _ => start + block_len,
                            }, total_bytes);
                            if end - start > block_len {
                                start + block_len
                            } else {
                                end
                            }
                        };
                        
                        Some(ByteRange::new(start, end))
                    })
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| RangeNotSatisfiable(ContentRange::unsatisfied_bytes(total_bytes)))?;

                eprintln!("{} satisfiable_ranges", satisfiable_ranges.len());
                if satisfiable_ranges.is_empty() {
                    eprintln!("No satisfiable ranges found");
                    if range_str == "bytes=0-0" {
                        eprintln!("bytes=0-0, returning zero-length range response");
                        // Special case for zero-length range request
                        let content_range = ContentRange::bytes(0..0, total_bytes)
                            .expect("ContentRange::bytes cannot panic in this usage");
                        let content_length = ContentLength(0);
                        let stream = RangedStream::new(self.body, 0, 0);
                        return Ok(RangedResponse::Single {
                            content_range,
                            content_length,
                            stream,
                        });
                    } else if range_str == "bytes=0-" {
                        let start = total_bytes.saturating_sub(1);
                        let content_range = ContentRange::bytes(start..total_bytes, total_bytes)
                            .expect("ContentRange::bytes cannot panic in this usage");
                        let content_length = ContentLength(total_bytes - start);
                        let stream = RangedStream::new(self.body, start, total_bytes - start);
                        return Ok(RangedResponse::Single {
                            content_range,
                            content_length,
                            stream,
                        });
                    } else {
                        if range_str.starts_with("bytes=") {
                            let bytes = range_str[6..].trim();
                            println!("bytes: {}", bytes);
                            // If the range is like "bytes=-n", we can return the last n bytes
                            let n: i64 = bytes.parse().unwrap();
                            if n < 0 {
                                eprintln!("Requested range is negative: {}, seeking from the end", n);
                                // but that's ok, we can just return the whole file, or the block size
                                // let n = total_bytes.wrapping_add(n as u64);
                                if -n > total_bytes as i64 {
                                    eprintln!("Requested range ({n}) exceeds total bytes, adjusting to total bytes ({total_bytes})");
                                    let content_range = ContentRange::bytes(0..total_bytes, total_bytes)
                                        .expect("ContentRange::bytes cannot panic in this usage");
                                    let content_length = ContentLength(total_bytes);
                                    let stream = RangedStream::new(self.body, 0, total_bytes);
                                    return Ok(RangedResponse::Single {
                                        content_range,
                                        content_length,
                                        stream,
                                    });
                                } else {
                                    todo!();
                                }
                            } else {
                                todo!();
                            }
                            // let start = total_bytes.saturating_sub((-n) as u64);
                            // let content_range = ContentRange::bytes(start..total_bytes, total_bytes)
                            //     .expect("ContentRange::bytes cannot panic in this usage");
                            // let content_length = ContentLength(n);
                            // let stream = RangedStream::new(self.body, start, n);
                            // return Ok(RangedResponse::Single {
                            //     content_range,
                            //     content_length,
                            //     stream,
                            // });
                        } else {
                            let bytes = range_str[6..].trim();
                            eprintln!("bytes: {}", bytes);
                            // if "bytes=-n" is requested, we can return the last n bytes up to the length of the file/block size
                            eprintln!("No satisfiable ranges found for range: {}", range_str);
                            todo!();
                        }
                    }
                    // TODO: try harder to match ranges that are RFC compliant
                    eprintln!("No satisfiable ranges found");
                    return Err(RangeNotSatisfiable(ContentRange::unsatisfied_bytes(total_bytes)));
                }

                if satisfiable_ranges.len() == 1 {
                    eprintln!("Single satisfiable range found: {:?}", satisfiable_ranges[0]);
                    // Single range response
                    let range = &satisfiable_ranges[0];
                    let content_range = ContentRange::bytes(range.start..range.end_exclusive, total_bytes)
                        .expect("ContentRange::bytes cannot panic in this usage");
                    let content_length = ContentLength(range.len());
                    let stream = RangedStream::new(self.body, range.start, range.len());

                    if content_range.bytes_len() == Some(content_length.0) {
                        // If the content length matches the range length, we can return 200 OK
                        Ok(RangedResponse::Full {
                            content_length,
                            stream,
                        })
                    } else {
                        // If the content length does not match, we return 206 Partial Content
                        Ok(RangedResponse::Single {
                            content_range,
                            content_length,
                            stream,
                        })
                    }
                } else {
                    // Multiple ranges response
                    let boundary = generate_boundary();
                    let multipart_stream = MultipartStream::new(self.body, satisfiable_ranges, total_bytes, boundary.clone());

                    Ok(RangedResponse::Multiple {
                        boundary,
                        stream: multipart_stream,
                    })
                }

            }
        }

        // // we don't support multiple byte ranges, only none or one
        // // fortunately, only responding with one of the requested ranges and
        // // no more seems to be compliant with the HTTP spec.
        // let range = self.range.and_then(|range| {
        //     range.satisfiable_ranges(total_bytes).nth(0)
        // });
        //
        // // pull seek positions out of range header
        // let seek_start = match range {
        //     Some((Bound::Included(seek_start), _)) => seek_start,
        //     _ => 0,
        // };
        //
        // let seek_end_excl = match range {
        //     // HTTP byte ranges are inclusive, so we translate to exclusive by adding 1:
        //     Some((_, Bound::Included(end))) => end + 1,
        //     _ => total_bytes,
        // };
        //
        // // check seek positions and return with 416 Range Not Satisfiable if invalid
        // let seek_start_beyond_seek_end = seek_start > seek_end_excl;
        // let seek_end_beyond_file_range = seek_end_excl > total_bytes;
        // // we could use >= above but I think this reads more clearly:
        // let zero_length_range = seek_start == seek_end_excl;
        //
        // if seek_start_beyond_seek_end || seek_end_beyond_file_range || zero_length_range {
        //     let content_range = ContentRange::unsatisfied_bytes(total_bytes);
        //     return Err(RangeNotSatisfiable(content_range));
        // }
        //
        // // if we're good, build the response
        // let content_range = range.map(|_| {
        //     ContentRange::bytes(seek_start..seek_end_excl, total_bytes)
        //         .expect("ContentRange::bytes cannot panic in this usage")
        // });
        //
        // let content_length = ContentLength(seek_end_excl - seek_start);
        //
        // let stream = RangedStream::new(self.body, seek_start, content_length.0);
        //
        // Ok(RangedResponse {
        //     content_range,
        //     content_length,
        //     stream,
        // })
    }
}

impl<B: RangeBody + Send + 'static> IntoResponse for Ranged<B> {
    fn into_response(self) -> Response {
        self.try_respond().into_response()
    }
}

/// Error type indicating that the requested range was not satisfiable. Implements [`IntoResponse`].
#[derive(Debug, Clone)]
pub struct RangeNotSatisfiable(pub ContentRange);

impl IntoResponse for RangeNotSatisfiable {
    fn into_response(self) -> Response {
        let status = StatusCode::RANGE_NOT_SATISFIABLE;
        let header = TypedHeader(self.0);
        (status, header, ()).into_response()
    }
}

/// Data type containing computed headers and body for a range response. Implements [`IntoResponse`].
pub enum RangedResponse<B> {
    /// Full content response, no range requested.
    Full {
        content_length: ContentLength,
        stream: RangedStream<B>,
    },
    Single {
        content_range: ContentRange,
        content_length: ContentLength,
        stream: RangedStream<B>,
    },
    Multiple {
        // content_ranges: Vec<ContentRange>,
        // content_length: ContentLength,
        stream: MultipartStream<B>,
        boundary: String,
    }
    // pub content_range: Option<ContentRange>,
    // pub content_length: ContentLength,
    // pub stream: RangedStream<B>,
}

impl<B: RangeBody + Send + 'static> IntoResponse for RangedResponse<B> {
    fn into_response(self) -> Response {
        // let mut headers = HeaderMap::new();
        // headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        // let mut headers = vec![];

        use RangedResponse::*;
        match self {
            Full { content_length, stream } => {
                // headers.push(("Content-Length", content_length.0.into()));
                let headers = [
                    ("Accept-Ranges", HeaderValue::from_static("bytes")),
                    ("Content-Length", content_length.0.into())
                ];
                (StatusCode::OK, headers, stream).into_response()
            }
            Single { content_range, content_length, stream } => {
                // headers.insert("Content-Range", TypedHeader(content_range).into());
                // headers.insert("Content-Length", content_length.0.into());
                let range = content_range
                    .bytes_range()
                    .map(|(start, end)| format!("bytes {}-{}/{}", start, end, content_length.0))
                    .unwrap();

                let headers = [
                    ("Accept-Ranges", HeaderValue::from_static("bytes")),
                    ("Content-Range", HeaderValue::from_str(&range).unwrap()),
                    ("Content-Length", content_length.0.into())
                ];
                if content_length.0 == 0 {
                    // If the content length is zero, we should return 204 No Content
                    (StatusCode::NO_CONTENT, headers, stream).into_response()
                } else if Some(content_length.0) != content_range.bytes_len() {
                    (StatusCode::PARTIAL_CONTENT, headers, stream).into_response()
                } else {
                    (StatusCode::OK, headers, stream).into_response()
                }
            }
            Multiple { /*content_ranges, content_length,*/ stream, boundary } => {
                // headers.insert("Content-Type", HeaderValue::from_str(&format!("multipart/byteranges; boundary={}", boundary)).unwrap());
                // headers.insert("Content-Length", content_length.0.into());

                let headers = [
                    ("Accept-Ranges", HeaderValue::from_static("bytes")),
                    ("Content-Type", HeaderValue::from_str(&format!("multipart/byteranges; boundary={}", boundary)).unwrap()),
                    // ("Content-Length", content_length.0.into())
                ];

                (StatusCode::PARTIAL_CONTENT, headers, stream).into_response()
            }
        }
        // let content_range = self.content_range.map(TypedHeader);
        // let content_length = TypedHeader(self.content_length);
        // let accept_ranges = TypedHeader(AcceptRanges::bytes());
        // let stream = self.stream;
        //
        // let status = match content_range {
        //     Some(_) => StatusCode::PARTIAL_CONTENT,
        //     None => StatusCode::OK,
        // };
        //
        // (status, content_range, content_length, accept_ranges, stream).into_response()
    }
}

/// generate a unique boundary string for multipart responses
fn generate_boundary() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_nanos();
    format!("RANGE_BOUNDARY-{}----", timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use axum::body::Body;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum_extra::headers::{ContentLength, ContentRange, Header};
    use bytes::Bytes;
    use futures::{pin_mut, Stream, StreamExt};
    use tokio::fs::File;

    use crate::{Ranged, RangedResponse};
    use crate::KnownSize;

    async fn collect_stream(stream: impl Stream<Item = io::Result<Bytes>>) -> String {
        let mut string = String::new();
        pin_mut!(stream);
        while let Some(chunk) = stream.next().await.transpose().unwrap() {
            string += std::str::from_utf8(&chunk).unwrap();
        }
        string
    }

    async fn collect_body_stream(body: impl Stream<Item = Result<Bytes, axum::Error>>) -> String {
        let mut string = String::new();
        pin_mut!(body);
        while let Some(chunk) = body.next().await.transpose().unwrap() {
            string += std::str::from_utf8(&chunk).unwrap();
        }
        string
    }

    fn range(header: &str) -> Option<Range> {
        let val = HeaderValue::from_str(header).unwrap();
        Some(Range::decode(&mut [val].iter()).unwrap())
    }

    async fn body() -> KnownSize<File> {
        let file = File::open("test/fixture.txt").await.unwrap();
        KnownSize::file(file).await.unwrap()
    }

    #[tokio::test]
    async fn test_full_response() {
        let ranged = Ranged::new(None, body().await);

        let r = ranged.try_respond().expect("try_respond should return Ok");
        let body = {
            let response = r.into_response();
            assert_eq!(StatusCode::OK, response.status());

            let head = response.headers();
            assert_eq!(Some(HeaderValue::from_static("bytes")).as_ref(), head.get("Accept-Ranges"));
            assert_eq!(Some(HeaderValue::from_static("54")).as_ref(), head.get("Content-Length"));
            // assert!(head.get("Content-Range").is_some());
            // assert_eq!(Some(HeaderValue::from_static("bytes 0-53/54")).as_ref(), head.get("Content-Range"));

            let b = response.into_body();
            b.into_data_stream()
        };

        let body = collect_body_stream(body).await;
        assert_eq!("Hello world this is a file to test range requests on!\n", body);
    }

    #[tokio::test]
    async fn test_partial_response_1() {
        let ranged = Ranged::new(range("bytes=0-29"), body().await);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(30), content_length);
                assert_eq!(ContentRange::bytes(0..30, 54).unwrap(), content_range);
                assert_eq!("Hello world this is a file to ", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_partial_response_2() {
        let ranged = Ranged::new(range("bytes=30-53"), body().await);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(24), content_length);
                assert_eq!(ContentRange::bytes(30..54, 54).unwrap(), content_range);
                assert_eq!("test range requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_unbounded_start_response() {
        // unbounded ranges in HTTP are actually a suffix

        let ranged = Ranged::new(range("bytes=-20"), body().await);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(20), content_length);
                assert_eq!(ContentRange::bytes(34..54, 54).unwrap(), content_range);
                assert_eq!(" range requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_unbounded_end_response() {
        let ranged = Ranged::new(range("bytes=40-"), body().await);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(14), content_length);
                assert_eq!(ContentRange::bytes(40..54, 54).unwrap(), content_range);
                assert_eq!(" requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_one_byte_response() {
        let ranged = Ranged::new(range("bytes=30-30"), body().await);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(1), content_length);
                assert_eq!(ContentRange::bytes(30..31, 54).unwrap(), content_range);
                assert_eq!("t", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_invalid_range() {
        let ranged = Ranged::new(range("bytes=30-29"), body().await);

        let err = ranged.try_respond().err().expect("try_respond should return Err");

        let expected_content_range = ContentRange::unsatisfied_bytes(54);
        assert_eq!(expected_content_range, err.0)
    }

    #[tokio::test]
    async fn test_range_end_exceed_length() {
        let ranged = Ranged::new(range("bytes=30-99"), body().await);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(24), content_length);
                assert_eq!(ContentRange::bytes(30..54, 54).unwrap(), content_range);
                assert_eq!("test range requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_range_start_exceed_length() {
        let ranged = Ranged::new(range("bytes=99-"), body().await);

        let err = ranged.try_respond().err().expect("try_respond should return Err");

        let expected_content_range = ContentRange::unsatisfied_bytes(54);
        assert_eq!(expected_content_range, err.0)
    }
}

#[cfg(test)]
mod rfc_compliance_tests {
    use std::io;
    use bytes::Bytes;
    use futures::{pin_mut, Stream, StreamExt};
    use axum::http::{HeaderValue, StatusCode};
    use axum_extra::headers::{ContentLength, ContentRange, Header, Range};
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::{Ranged, KnownSize, RangedResponse, RangeNotSatisfiable, stream};

    // Utility functions for tests
    async fn collect_stream(stream: impl Stream<Item = io::Result<Bytes>>) -> String {
        let mut string = String::new();
        pin_mut!(stream);
        while let Some(chunk) = stream.next().await.transpose().unwrap() {
            string += std::str::from_utf8(&chunk).unwrap();
        }
        string
    }

    async fn collect_multipart_stream(stream: impl Stream<Item = io::Result<Bytes>>, boundary: &str) -> String {
        let mut content = String::new();
        let mut buffer = String::new();
        let full_boundary = format!("--{}", boundary);
        let end_boundary = format!("--{}--", boundary);

        pin_mut!(stream);
        while let Some(chunk) = stream.next().await.transpose().unwrap() {
            buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        }

        // Split by boundary
        let parts: Vec<&str> = buffer.split(&full_boundary).collect();

        // Process each part (skip the first as it's usually empty)
        for part in parts.iter().skip(1) {
            // Skip the end boundary marker
            if part.trim().starts_with("--\r\n") || part.is_empty() {
                continue;
            }

            // Find the double CRLF that separates headers from body
            if let Some(idx) = part.find("\r\n\r\n") {
                // Extract the body content (skip the headers)
                let body = &part[idx + 4..];
                // Remove the trailing CRLF if present
                let body = body.trim_end_matches("\r\n");
                content.push_str(body);
            }
        }

        content
    }

    fn range(header: &str) -> Option<Range> {
        let val = HeaderValue::from_str(header).unwrap();
        Some(Range::decode(&mut [val].iter()).unwrap())
    }

    async fn create_test_file(path: &str, content: &str) -> io::Result<()> {
        let mut file = File::create(path).await?;
        file.write_all(content.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    async fn body(path: &str) -> KnownSize<File> {
        // sleep for a random amount of time to ensure the file is created before reading
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Open the file and wrap it in KnownSize
        let file = File::open(path).await.unwrap();
        KnownSize::file(file).await.unwrap()
    }

    async fn body_blocks(path: &str, block_len: u64) -> KnownSize<File> {
        // sleep for a random amount of time to ensure the file is created before reading
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Open the file and wrap it in KnownSize with specified block length
        let file = File::open(path).await.unwrap();
        KnownSize::file_blocks(file, block_len).await.unwrap()
    }

    // Test setup that creates a test file with known content
    async fn setup() -> io::Result<String> {
        let test_path = "test/fixture_abc.txt";
        let content = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        create_test_file(test_path, content).await?;
        Ok(test_path.to_string())
    }

    // 1. Test: Full response without range
    #[tokio::test]
    async fn test_full_response() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(None, body(&test_path).await);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Full { content_length, stream } => {
                assert_eq!(ContentLength(62), content_length);
                // assert_eq!(ContentRange::bytes(0..62, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", &content);
            }
            _ => panic!("Expected a full range response"),
        }
        Ok(())
    }

    // 2. Test: Single byte range
    #[tokio::test]
    async fn test_single_byte_range() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=0-9"), body(&test_path).await);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(10), content_length);
                assert_eq!(ContentRange::bytes(0..10, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("0123456789", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        // assert_eq!(10, response.content_length.0);
        // let expected_content_range = ContentRange::bytes(0..10, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("0123456789", &content);
        Ok(())
    }

    // 3. Test: Range with end position beyond content length
    // This should return the available bytes from start to end, not an error
    #[tokio::test]
    async fn test_range_end_exceeds_length() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=50-999"), body(&test_path).await);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(12), content_length);
                assert_eq!(ContentRange::bytes(50..62, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("opqrstuvwxyz", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        // assert_eq!(12, response.content_length.0); // From position 50 to the end (61)
        // let expected_content_range = ContentRange::bytes(50..62, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("mnopqrstuvwxyz", &content);
        Ok(())
    }

    // 4. Test: Range with start position beyond content length
    #[tokio::test]
    async fn test_range_start_exceeds_length() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=100-"), body(&test_path).await);
        let result = ranged.try_respond();

        assert!(result.is_err());
        let err = result.err().unwrap();
        let expected_content_range = ContentRange::unsatisfied_bytes(62);
        assert_eq!(expected_content_range, err.0);
        Ok(())
    }

    // 5. Test: Suffix range
    #[tokio::test]
    async fn test_suffix_range() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=-10"), body(&test_path).await);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(10), content_length);
                assert_eq!(ContentRange::bytes(52..62, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("qrstuvwxyz", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        // assert_eq!(10, response.content_length.0);
        // let expected_content_range = ContentRange::bytes(52..62, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("nopqrstuvwxyz", &content);
        Ok(())
    }

    // 6. Test: Suffix range larger than content
    #[tokio::test]
    async fn test_suffix_range_exceeds_length() -> io::Result<()> {
        let test_path = setup().await?;
        let body = body(&test_path).await;
        let ranged = Ranged::new(range("bytes=-100"), body);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(62), content_length);
                assert_eq!(ContentRange::bytes(0..62, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        // assert_eq!(62, response.content_length.0); // Should return entire content
        // let expected_content_range = ContentRange::bytes(0..62, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", &content);
        Ok(())
    }

    // 7. Test: Range with start > end
    #[tokio::test]
    async fn test_range_start_greater_than_end() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=30-20"), body(&test_path).await);
        let result = ranged.try_respond();

        assert!(result.is_err());
        let err = result.err().unwrap();
        let expected_content_range = ContentRange::unsatisfied_bytes(62);
        assert_eq!(expected_content_range, err.0);
        Ok(())
    }

    // 8. Test: First and last byte only
    #[tokio::test]
    async fn test_first_and_last_byte() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=0-0,-1"), body(&test_path).await);

        println!("Testing first and last byte range : ranged={:?}", ranged);

        // Since our implementation doesn't support multiple ranges yet,
        // it should only return the first range
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Multiple { stream, boundary } => {
                assert_eq!("RANGE_BOUNDARY-", &boundary[..15]);
                let content = collect_multipart_stream(stream, &boundary).await;
                assert_eq!("0z", &content); // First byte is '0', last byte is 'z'
            }
            _ => panic!("Expected a multiple range response"),
        }

        // assert_eq!(1, response.content_length.0);
        // let expected_content_range = ContentRange::bytes(0..1, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("0", &content);
        Ok(())
    }

    // 9. Test: Multiple ranges support (future implementation)
    // For now this should return the first range only
    #[tokio::test]
    async fn test_multiple_ranges() -> io::Result<()> {
        let test_path = setup().await?;
        let body = body(&test_path).await;
        let ranged = Ranged::new(range("bytes=0-9,20-29,40-49"), body);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Multiple { stream, boundary } => {
                // For now we only support the first range, so we expect a single range response
                assert!(boundary.starts_with("RANGE_BOUNDARY-"));
                let content = collect_multipart_stream(stream, &boundary).await;
                assert_eq!("0123456789KLMNOPQRSTefghijklmn", &content);
            }
            _ => panic!("Expected a multiple range response"),
        }

        // // Currently our implementation only supports the first range
        // assert_eq!(10, response.content_length.0);
        // let expected_content_range = ContentRange::bytes(0..10, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("0123456789", &content);
        Ok(())
    }

    // 10. Test: Zero length range
    #[tokio::test]
    async fn test_zero_length_range() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=10-9"), body(&test_path).await);
        let result = ranged.try_respond();

        assert!(result.is_err());
        let err = result.err().unwrap();
        let expected_content_range = ContentRange::unsatisfied_bytes(62);
        assert_eq!(expected_content_range, err.0);
        Ok(())
    }

    // 11. Test: Range with only a start position
    #[tokio::test]
    async fn test_range_only_start() -> io::Result<()> {
        let test_path = setup().await?;
        let ranged = Ranged::new(range("bytes=50-"), body(&test_path).await);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(12), content_length);
                assert_eq!(ContentRange::bytes(50..62, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("opqrstuvwxyz", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        // assert_eq!(12, response.content_length.0); // From position 50 to the end (61)
        // let expected_content_range = ContentRange::bytes(50..62, 62).unwrap();
        // assert_eq!(Some(expected_content_range), response.content_range);
        // let content = collect_stream(response.stream).await;
        // assert_eq!("mnopqrstuvwxyz", &content);
        Ok(())
    }

    // 12. Test: Invalid range format
    #[tokio::test]
    async fn test_invalid_range_format() -> io::Result<()> {
        let test_path = setup().await?;
        let body = body(&test_path).await;
        // This wouldn't parse as a valid Range header, so we'd get None
        let ranged = Ranged::new(None, body);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Full { content_length, stream } => {
                assert_eq!(ContentLength(62), content_length);
                let content = collect_stream(stream).await;
                assert_eq!("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", &content);
            }
            RangedResponse::Single { content_range, content_length, stream } => {
                panic!("Expected a full response, got single range response");
            }
            RangedResponse::Multiple { stream, boundary } => {
                panic!("Expected a full response, got multiple ranges response");
            }
            // _ => panic!("Expected a full response"),
        }

        // assert_eq!(62, response.content_length.0);
        // assert!(response.content_range.is_none());
        // let content = collect_stream(response.stream).await;
        // assert_eq!("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", &content);
        Ok(())
    }

    #[tokio::test]
    async fn test_block_ranges() -> io::Result<()> {
        let test_path = setup().await?;
        let block_size = 10;
        let body = body_blocks(&test_path, block_size).await;

        // Test with block size of 10
        let ranged = Ranged::new(range("bytes=0-29"), body);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(10), content_length);
                assert_eq!(ContentRange::bytes(0..10, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("0123456789", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_block_ranges_mid() -> io::Result<()> {
        let test_path = setup().await?;
        let block_size = 10;
        let body = body_blocks(&test_path, block_size).await;

        // Test with block size of 10
        let ranged = Ranged::new(range("bytes=10-29"), body);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream } => {
                assert_eq!(ContentLength(10), content_length);
                assert_eq!(ContentRange::bytes(10..20, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("ABCDEFGHIJ", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        Ok(())
    }
}

