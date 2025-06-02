#![allow(unused)]
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
//! use std::path::PathBuf;
//! use std::str::FromStr;
//!
//! use axum_range::Ranged;
//! use axum_range::KnownSize;
//!
//! async fn file(range: Option<TypedHeader<Range>>) -> Ranged<KnownSize<tokio::fs::File>> {
//! let file = PathBuf::from_str("document.txt").unwrap();
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
pub use stream::{RangedStream, MultipartStream, extract_boundary};

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
    content_type: Option<String>,
}

impl<B: RangeBody + Send + 'static> Ranged<B> {
    /// Construct a ranged response over any type implementing [`RangeBody`]
    /// and an optional [`Range`] header.
    pub fn new(range: Option<Range>, body: B, content_type: Option<String>) -> Self {
        Ranged { range, body, content_type }
    }

    /// Responds to the request, returning headers and body as
    /// [`RangedResponse`]. Returns [`RangeNotSatisfiable`] error if requested
    /// range in header was not satisfiable.
    pub fn try_respond(self) -> Result<RangedResponse<B>, RangeNotSatisfiable> {
        let total_bytes = self.body.byte_size();
        if total_bytes == 0 {
            println!("total bytes = 0!");
            return Err(RangeNotSatisfiable(ContentRange::unsatisfied_bytes(total_bytes)));
        }

        let block_len = self.body.block_len();

        match self.range {
            None => {
                if total_bytes <= block_len {
                    eprintln!("No range header, returning full file of size: {}", total_bytes);
                    // no range header, return the whole file
                    let content_length = ContentLength(total_bytes);
                    let stream = RangedStream::new(self.body, 0, total_bytes);
                    let content_type = self.content_type;
                    Ok(RangedResponse::Full {
                        content_length,
                        stream,
                        content_type,
                    })
                } else {
                    eprintln!("No range header, returning first {} bytes of file of size: {}", block_len, total_bytes);
                    // block_len is less than total_bytes, return the first block_len bytes
                    let content_length = ContentLength(total_bytes);
                    let stream = RangedStream::new(self.body, 0, block_len);
                    let content_range = ContentRange::bytes(0..block_len, total_bytes)
                        .expect("ContentRange::bytes cannot panic in this usage");
                    let content_type = self.content_type;
                    Ok(RangedResponse::Single {
                        content_range,
                        content_length,
                        stream,
                        content_type,
                    })
                }
            }
            Some(range) => {
                /*
                   A byte-range-spec is invalid if the last-byte-pos value is present
                   and less than the first-byte-pos.

                   A client can limit the number of bytes requested without knowing the
                   size of the selected representation.  If the last-byte-pos value is
                   absent, or if the value is greater than or equal to the current
                   length of the representation data, the byte range is interpreted as
                   the remainder of the representation (i.e., the server replaces the
                   value of last-byte-pos with a value that is one less than the current
                   length of the selected representation).


                   A client can request the last N bytes of the selected representation
                   using a suffix-byte-range-spec.

                     suffix-byte-range-spec = "-" suffix-length
                     suffix-length = 1*DIGIT

                   If the selected representation is shorter than the specified
                   suffix-length, the entire representation is used.

                   Additional examples, assuming a representation of length 10000:

                   o  The final 500 bytes (byte offsets 9500-9999, inclusive):

                        bytes=-500

                   Or:

                        bytes=9500-

                   o  The first and last bytes only (bytes 0 and 9999):

                        bytes=0-0,-1

                   o  Other valid (but not canonical) specifications of the second 500
                      bytes (byte offsets 500-999, inclusive):

                        bytes=500-600,601-999
                        bytes=500-700,601-999

                */

                println!("range: {:?}", range);
                // hacky way to get the range string for parsing
                let mut range_str = format!("{:?}", range).replace("Range(\"", "").replace("\")", "");
                println!("total_bytes: {}", total_bytes);
                let ranges = parse_range_header(&range_str, total_bytes)
                    .map_err(|e| {
                        eprintln!("Error parsing range header: {:?}", e);
                        RangeNotSatisfiable(ContentRange::unsatisfied_bytes(total_bytes))
                    })?;
                println!("Parsed ranges: {:?}", ranges);
                // let parsed_ranges = parse_ranges(range_str.clone())
                //     .map_err(|e|RangeNotSatisfiable(ContentRange::unsatisfied_bytes(total_bytes)))?;
                // // count all the commas in the range string
                let range_ct = range_str.as_str()
                    .chars()
                    .filter(|&c| c == ',')
                    .count() + 1;
                // if parsed_ranges.0.len() != range_ct {
                //     eprintln!("Parsed ranges count {} does not match expected count {}", parsed_ranges.0.len(), range_ct);
                //     return Err(RangeNotSatisfiable(ContentRange::unsatisfied_bytes(total_bytes)));
                // }
                /*
                    Satisfiability:
                    If a valid byte-range-set includes at least one byte-range-spec with
                    a first-byte-pos that is less than the current length of the
                    representation, or at least one suffix-byte-range-spec with a
                    non-zero suffix-length, then the byte-range-set is satisfiable.
                    Otherwise, the byte-range-set is unsatisfiable.
                 */
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

                eprintln!("{} satisfiable_ranges {} expected", satisfiable_ranges.len(), range_ct);
                let satisfiable_ranges = if satisfiable_ranges.len() != range_ct {
                    eprintln!("Number of satisfiable ranges does not match expected count");
                    let range_strs = satisfiable_ranges.iter()
                        .map(|r| format!("{}-{}", r.start, r.end_exclusive - 1))
                        .collect::<Vec<_>>();
                        // .join(",");
                    eprintln!("Satisfiable ranges: {:?}", range_strs);
                    // new range string with the satisfiable ranges removed, so we can process the rest
                    let mut undetected_ranges = Vec::new();
                    for range in satisfiable_ranges.iter() {
                        if range_str.contains(&format!("{}-{}", range.start, range.end_exclusive - 1)) {
                            println!("Removing found range {}-{} from range string", range.start, range.end_exclusive - 1);
                            // remove the range from the string
                            range_str = range_str.replace(&format!("{}-{}", range.start, range.end_exclusive - 1), "");
                        } else {
                            // if the range is not in the string, we can just add it to the new string
                            undetected_ranges.push(format!("{},", range_str));
                        }
                    }
                    range_str = if range_str.ends_with(',') {
                        range_str[..range_str.len() - 1].to_string()
                    } else {
                        range_str
                    };
                    range_str = if range_str.starts_with(',') {
                        range_str[1..].to_string()
                    } else {
                        range_str
                    };
                    println!("Undetected ranges: {} {}", undetected_ranges.join(","), range_str);
                    satisfiable_ranges
                } else {
                    satisfiable_ranges
                };

                let content_type = self.content_type;
                if satisfiable_ranges.is_empty() {
                    eprintln!("No satisfiable ranges found");
                    if range_str == "bytes=0-0" {
                        eprintln!("bytes=0-0, returning zero-length range response");
                        // Special case for zero-length range request
                        let content_range = ContentRange::bytes(0..0, total_bytes)
                            .expect("ContentRange::bytes cannot panic in this usage");
                        let content_length = ContentLength(total_bytes);
                        let stream = RangedStream::new(self.body, 0, 0);
                        return Ok(RangedResponse::Single {
                            content_range,
                            content_length,
                            stream,
                            content_type,
                        });
                    } else if range_str == "bytes=0-" {
                        let start = total_bytes.saturating_sub(1);
                        let content_range = ContentRange::bytes(start..total_bytes, total_bytes)
                            .expect("ContentRange::bytes cannot panic in this usage");
                        let content_length = ContentLength(total_bytes);
                        let stream = RangedStream::new(self.body, start, total_bytes - start);
                        return Ok(RangedResponse::Single {
                            content_range,
                            content_length,
                            stream,
                            content_type,
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
                                        content_type,
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
                    let content_length = ContentLength(total_bytes);
                    let stream = RangedStream::new(self.body, range.start, range.len());
                    eprintln!("Single satisfiable range found: {} / {}", range.len(), total_bytes);

                    if Some(range.end_exclusive - range.start) == Some(content_length.0) {
                        // If the content length matches the range length, we can return 200 OK
                        Ok(RangedResponse::Full {
                            content_length,
                            stream,
                            content_type,
                        })
                    } else {
                        // If the content length does not match, we return 206 Partial Content
                        Ok(RangedResponse::Single {
                            content_range,
                            content_length,
                            stream,
                            content_type,
                        })
                    }
                } else {
                    // Multiple ranges response
                    let boundary = generate_boundary();
                    let multipart_stream = MultipartStream::new(self.body, satisfiable_ranges, total_bytes, boundary.clone(), content_type.clone());
                    
                    let content_length = ContentLength(total_bytes);
                    Ok(RangedResponse::Multiple {
                        boundary,
                        stream: multipart_stream,
                        content_length,
                        content_type,
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

#[derive(Debug, Clone, PartialEq)]
enum ParseRangesError {
    InvalidRange(String),
    InvalidByteRange(String),
    InvalidSuffixRange(String),
}

fn parse_ranges(range_str: String) -> Result<(Vec<(Bound<i64>, Bound<i64>)>, Option<Option<u64>>), ParseRangesError> {
    // let mut ranges = Vec::new();
    // let mut suffix_length = None;
    let mut range_secs = range_str.trim().split("/");
    if let Some(range_str) = range_secs.next() {
        // If the range string is empty, return an error
        if range_str.is_empty() {
            return Err(ParseRangesError::InvalidRange(range_str.to_string()));
        }
        // TODO: parse the ranges string
    } else {
        return Err(ParseRangesError::InvalidRange(range_str.to_string()));
    }
    if let Some(exp_total_len) = range_secs.next() {
        // If the expected total length is not a valid number, return an error
        if let Ok(len) = exp_total_len.parse::<u64>() {
            // If the length is zero, we can return an empty range
            if len == 0 {
                return Ok((Vec::new(), Some(None)));
            }
        } else {
            return Err(ParseRangesError::InvalidRange(exp_total_len.to_string()));
        }
    } else {
        // If there is no expected total length, the client does not know the total length
    }
    // let [be, len] = range_str.collect::<Vec<_>>()[..] else {
    //     return Err(ParseRangesError::InvalidRange(range_str.to_string()));
    // };
    // for section in range_str {
    //     if section.trim().is_empty() {
    //         continue;
    //     }
    //     // If the section is not a valid range, return an error
    //     if !section.starts_with("bytes=") && !section.starts_with("bytes=-") {
    //         return Err(ParseRangesError::InvalidRange(section.to_string()));
    //     }
    // }
    // for range in range_str.split(',') {
    //     let range = range.trim();
    //     if range.is_empty() {
    //         continue;
    //     }
    //     if range.starts_with("bytes=") {
    //         let bytes = &range[6..];
    //         if bytes.contains('-') {
    //             let parts: Vec<&str> = bytes.split('-').collect();
    //             if parts.len() != 2 {
    //                 return Err(ParseRangesError::InvalidRange(range.to_string()));
    //             }
    //             let start = parts[0].parse::<i64>().map_err(|_| ParseRangesError::InvalidByteRange(range.to_string()))?;
    //             let end = parts[1].parse::<i64>().map_err(|_| ParseRangesError::InvalidByteRange(range.to_string()))?;
    //             ranges.push((Bound::Included(start), Bound::Included(end)));
    //         } else {
    //             return Err(ParseRangesError::InvalidRange(range.to_string()));
    //         }
    //     } else if range.starts_with("bytes=-") {
    //         let suffix_len = &range[7..];
    //         let suffix_length_value = suffix_len.parse::<u64>().map_err(|_| ParseRangesError::InvalidSuffixRange(range.to_string()))?;
    //         suffix_length = Some(Some(suffix_length_value));
    //     } else {
    //         return Err(ParseRangesError::InvalidRange(range.to_string()));
    //     }
    // }
    // println!("Parsed ranges: {:?}", ranges);
    // if ranges.is_empty() && suffix_length.is_none() {
    //     return Err(ParseRangesError::InvalidRange(range_str));
    // }
    // Ok((ranges, suffix_length))
    todo!();
}

trait RangeParser {
    fn supported_ranges(&self) -> Vec<String>;
}

fn parse_range_header(range_header: &str, target_size: u64) -> Result<Vec<(Bound<u64>, Bound<u64>)>, ParseRangesError> {
    println!("Parsing range header ({target_size}): {range_header}");
    let end_index = target_size - 1;
    if range_header.is_empty() {
        return Ok(vec![(Bound::Included(0), Bound::Included(end_index))]);
    }

    let bytes_ = "bytes=";
    if !range_header.starts_with(bytes_) {
        eprintln!("Range header does not start with '{}'", bytes_);
        return Err(ParseRangesError::InvalidRange(range_header.to_string()));
    }

    let mut ranges = Vec::new();
    for range in range_header[bytes_.len()..].split(',') {
        let split: Vec<&str> = range.split('-').collect();
        match split.len() {
            1 => {
                println!("Single range: {}", split[0]);
                match split[0].parse::<u64>() {
                    Ok(start) => {
                        if start > end_index {
                            eprintln!("Start position {} exceeds end index {}", start, end_index);
                            return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                        }
                        ranges.push((Bound::Included(start), Bound::Included(end_index)));
                    }
                    Err(_) => {
                        match split[0].parse::<i64>() {
                            Ok(start) if start < 0 => {
                                let start = end_index.saturating_sub((-start) as u64);
                                ranges.push((Bound::Included(start), Bound::Unbounded));
                            }
                            _ => {
                                eprintln!("Start position {} is not valid", split[0]);
                                return Err(ParseRangesError::InvalidByteRange(range.to_string()))
                            }
                        }
                    },
                }
                // let start = split[0].parse::<u64>().map_err(|_| ParseRangesError::InvalidByteRange(range.to_string()))?;
                // ranges.push((Bound::Included(start), Bound::Included(end_index)));
            }
            2 => {
                eprintln!("Range with start and end: '{}'-'{}'", split[0], split[1]);
                let start = if split[0].is_empty() {
                    println!("split[0] is empty");
                    // parse ranges of the form "bytes=-100" (i.e., last 100 bytes)
                    if split[1].is_empty() {
                        eprintln!("Invalid range format: {}", range);
                        return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                    } else {
                        println!("Last bytes range: {}", split[1]);
                        let split: Vec<&str> = split[1].split('/').collect();
                        match split[0].parse::<u64>() {
                            Ok(start) => {
                                let end = end_index;
                                // If the end is negative, we calculate the start position from the end index
                                eprintln!("Start position: {start} (end index: {end_index})");
                                let (start, end) = if start > end_index {
                                    eprintln!("Start position {} exceeds end index {}", start, end_index);
                                    return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                                } else {
                                    (end_index.saturating_sub(start), end_index)
                                };
                                ranges.push((Bound::Included(start+1), Bound::Unbounded));
                                continue;
                            }
                            _ => {
                                eprintln!("End position {} is not valid", split[1]);
                                return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                            }
                        }
                        // end_index.saturating_sub(split[1].parse::<u64>()
                        //     .inspect_err(|e| eprintln!("Failed to parse end index from '{}': {}", split[1], e))
                        //     .map_err(|_| ParseRangesError::InvalidByteRange(range.to_string()))?)
                    }
                } else {
                    split[0].parse::<u64>()
                        .inspect_err(|e| eprintln!("Failed to parse start index from '{}': {}", split[0], e))
                        .map_err(|_| ParseRangesError::InvalidByteRange(range.to_string()))?
                };
                let end = if split[1].is_empty() {
                    end_index
                } else {
                    if split[0].is_empty() {
                        let s = format!("-{}", split[1]);
                        let split = s.split("/").collect::<Vec<_>>();
                        match split[0].parse::<i64>() {
                            Ok(end) => {
                                if -end > end_index as i64 {
                                    end_index
                                } else {
                                    end_index - ((-end) as u64)
                                }
                            },
                            Err(e) => {
                                eprintln!("Could not parse int: {} ; {}", split[0], range);
                                return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                            }
                        }
                    } else {
                        println!("Range with start and end: {}->{}", split[0], split[1]);
                        println!("Last bytes range: {}", split[1]);
                        let split: Vec<&str> = split[1].split('/').collect();
                        match split[0].parse::<u64>() {
                            Ok(end) => {
                                // let end = end_index;
                                // If the end is negative, we calculate the start position from the end index
                                eprintln!("Start position: {start} (end index: {end_index})");
                                let (start, end) = if start > end_index {
                                    eprintln!("Start position {} exceeds end index {}", start, end_index);
                                    return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                                } else {
                                    (end_index.saturating_sub(start), end_index)
                                };
                                ranges.push((Bound::Included(start+1), Bound::Unbounded));
                                continue;
                            }
                            _ => {
                                eprintln!("End position {} is not valid", split[1]);
                                return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                            }
                        }
                        // match split[1].parse::<u64>() {
                        //     Ok(end) => end,
                        //     Err(_) => {
                        //         match split[1].parse::<i64>() {
                        //             Ok(end) if end < 0 => end_index.saturating_sub((-end) as u64),
                        //             _ => {
                        //                 eprintln!("End position {} is not valid", split[1]);
                        //                 return Err(ParseRangesError::InvalidByteRange(range.to_string()))
                        //             },
                        //         }
                        //     }
                        // }
                        // split[1].parse::<u64>().map_err(|_| ParseRangesError::InvalidByteRange(range.to_string()))?
                    }
                };
                if end < start {
                    return Err(ParseRangesError::InvalidByteRange(range.to_string()));
                }
                ranges.push((Bound::Included(start), Bound::Included(end.min(end_index))));
            }
            _ => {
                eprintln!("Invalid range format: {}", range);
                return Err(ParseRangesError::InvalidRange(range.to_string()))
            },
        }
    }

    // merge the ranges
    let mut merged: Vec<(Bound<u64>, Bound<u64>)> = vec![];
    ranges.sort_by_key(|r| match r.0{
        Bound::Included(start) => start,
        Bound::Excluded(start) => start,
        Bound::Unbounded => 0, // Unbounded should be treated as the lowest possible start
    });
    for range in ranges {
        let start = match range.0 {
            Bound::Included(start) => start,
            Bound::Excluded(start) => start - 1,
            Bound::Unbounded => 0, // Unbounded should be treated as the lowest possible start
        };
        let end_inc = match range.1 {
            Bound::Included(end) => end,
            Bound::Excluded(end) => end - 1,
            Bound::Unbounded => end_index, // Unbounded should be treated as the highest possible end
        };
        if merged.is_empty() {
            merged.push((Bound::Included(start), Bound::Included(end_inc)));
            continue;
        }
        match &mut merged.last_mut().unwrap().1 {
            Bound::Included(last_end) if &start > last_end => {
                merged.push((Bound::Included(start), Bound::Included(end_inc)));
            }
            Bound::Included(last_end) => {
                // If the start is less than or equal to the last end, we merge the ranges
                *last_end = *(&*last_end).max(&end_inc);
            }
            Bound::Excluded(last_end) if &start >= last_end => {
                merged.push((Bound::Included(start), Bound::Included(end_inc)));
            }
            Bound::Excluded(last_end) => {
                // If the start is less than or equal to the last end, we merge the ranges
                *last_end = (*last_end).max(end_inc + 1);
            }
            Bound::Unbounded => break, // Unbounded end means we've already merge all possible rangesz
        }
    }

    Ok(merged)
}

#[test]
fn test_parse_range_header() {
    let tests = [
        ("bytes=0-100", 200, Ok(vec![(Bound::Included(0), Bound::Included(100))])),
        ("bytes=0-100,200-300", 500, Ok(vec![
            (Bound::Included(0), Bound::Included(100)),
            (Bound::Included(200), Bound::Included(300))
        ])),
        ("bytes=0-", 500, Ok(vec![(Bound::Included(0), Bound::Included(499))])),
        ("bytes=-100", 500, Ok(vec![(Bound::Included(400), Bound::Included(499))])),
        ("bytes=100-", 500, Ok(vec![(Bound::Included(100), Bound::Included(499))])),
        ("bytes=100-200", 500, Ok(vec![(Bound::Included(100), Bound::Included(200))])),
        ("bytes=100-200,300-400", 500, Ok(vec![
            (Bound::Included(100), Bound::Included(200)),
            (Bound::Included(300), Bound::Included(400))
        ])),
        ("bytes=-1", 500, Ok(vec![(Bound::Included(499), Bound::Included(499))])),
        ("bytes=0-0", 500, Ok(vec![(Bound::Included(0), Bound::Included(0))])),
        ("bytes=0-0,-1", 500, Ok(vec![
            (Bound::Included(0), Bound::Included(0)),
            (Bound::Included(499), Bound::Included(499))
        ])),
        ("bytes=0-4,-1", 500, Ok(vec![
            (Bound::Included(0), Bound::Included(4)),
            (Bound::Included(499), Bound::Included(499))
        ])),
        ("bytes=0-4,-1/*", 500, Ok(vec![
            (Bound::Included(0), Bound::Included(4)),
            (Bound::Included(499), Bound::Included(499))
        ])),
        ("bytes=0-24646", 500, Ok(vec![(Bound::Included(0), Bound::Included(499))])),
        ("bytes=0-24646/*", 500, Ok(vec![(Bound::Included(0), Bound::Included(499))])),
        ("bytes=0-24646", 500, Ok(vec![(Bound::Included(0), Bound::Included(499))])),
        ("bytes=0-24646/25000", 500, Ok(vec![(Bound::Included(0), Bound::Included(499))])),
        ("none", 500, Err(ParseRangesError::InvalidRange("none".to_string()))),
        ("bleets=100-324", 500, Err(ParseRangesError::InvalidRange("bleets=100-324".to_string()))),
        ("bytes=0-24646,100-200", 500, Ok(vec![
            (Bound::Included(0), Bound::Included(499))
        ])),
        ("bytes=0-24646,100-200,300-400", 500, Ok(vec![
            (Bound::Included(0), Bound::Included(499)),
        ])),
    ];

    for (i, (range_header, target_size, expected)) in tests.iter().enumerate() {
        let result = parse_range_header(range_header, *target_size);
        assert_eq!(result, *expected, "Failed to parse range header #{i}: {}\n{:?}", range_header, tests[i]);
    }
}

#[test]
fn test_parse_ranges() {
    let range_str = "bytes=0-100,200-300,400-500".to_string();
    println!("Ranges to parse: {}", range_str);
    let (ranges, suffix_length) = parse_ranges(range_str).unwrap();
    assert_eq!(ranges.len(), 3);
    assert_eq!(suffix_length, None);
    assert_eq!(ranges[0], (Bound::Included(0), Bound::Included(101)));
    assert_eq!(ranges[1], (Bound::Included(200), Bound::Included(301)));
    assert_eq!(ranges[2], (Bound::Included(400), Bound::Included(501)));

    let range_str = "bytes=-100".to_string();
    let (ranges, suffix_length) = parse_ranges(range_str).unwrap();
    assert_eq!(ranges.len(), 0);
    assert_eq!(suffix_length, Some(Some(100)));

    let range_str = "bytes=0-100".to_string();
    let (ranges, suffix_length) = parse_ranges(range_str).unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!(suffix_length, None);
    assert_eq!(ranges[0], (Bound::Included(0), Bound::Included(101)));
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

#[derive(Debug)]
/// Data type containing computed headers and body for a range response. Implements [`IntoResponse`].
pub enum RangedResponse<B> {
    /// Full content response, no range requested.
    Full {
        content_length: ContentLength,
        stream: RangedStream<B>,
        content_type: Option<String>,
    },
    Single {
        content_range: ContentRange,
        content_length: ContentLength,
        stream: RangedStream<B>,
        content_type: Option<String>,
    },
    Multiple {
        // content_ranges: Vec<ContentRange>,
        stream: MultipartStream<B>,
        boundary: String,
        content_length: ContentLength,
        content_type: Option<String>,
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
            Full { content_length, stream, content_type } => {
                // headers.push(("Content-Length", content_length.0.into()));
                let headers = [
                    ("Accept-Ranges", HeaderValue::from_static("bytes")),
                    ("Content-Length", content_length.0.into()),
                    ("Content-Type", HeaderValue::from_str(&content_type.unwrap_or_else(|| "application/octet-stream".to_string())).unwrap())
                ];
                (StatusCode::OK, headers, stream).into_response()
            }
            Single { content_range, content_length, stream, content_type } => {
                // headers.insert("Content-Range", TypedHeader(content_range).into());
                // headers.insert("Content-Length", content_length.0.into());
                let range = content_range
                    .bytes_range()
                    .map(|(start, end)| format!("bytes {}-{}/{}", start, end, content_length.0))
                    .unwrap();

                let headers = [
                    ("Accept-Ranges", HeaderValue::from_static("bytes")),
                    ("Content-Range", HeaderValue::from_str(&range).unwrap()),
                    // ("Content-Length", content_length.0.into()), // this causes problems if included
                    ("Content-Type", HeaderValue::from_str(&content_type.unwrap_or_else(|| "application/octet-stream".to_string())).unwrap())
                ];
                if content_length.0 == 0 {
                    // If the content length is zero, we should return 204 No Content
                    (StatusCode::NO_CONTENT, headers, stream).into_response()
                } else if Some(content_length.0) != content_range.bytes_range().map(|(start, end)| end-start) {
                    (StatusCode::PARTIAL_CONTENT, headers, stream).into_response()
                } else {
                    (StatusCode::OK, headers, stream).into_response()
                }
            }
            Multiple { /*content_ranges, content_length,*/ stream, boundary, content_type, content_length } => {
                // headers.insert("Content-Type", HeaderValue::from_str(&format!("multipart/byteranges; boundary={}", boundary)).unwrap());
                // headers.insert("Content-Length", content_length.0.into());

                let headers = [
                    ("Accept-Ranges", HeaderValue::from_static("bytes")),
                    ("Content-Type", HeaderValue::from_str(&format!("multipart/byteranges; boundary={}", boundary)).unwrap()),
                    ("Content-Length", content_length.0.into())
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
    use std::path::PathBuf;
    use std::str::FromStr;
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
        let file = PathBuf::from_str("test/fixture.txt").unwrap();
        KnownSize::file(file).await.unwrap()
    }

    #[tokio::test]
    async fn test_full_response() {
        let ranged = Ranged::new(None, body().await, None);

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
        let ranged = Ranged::new(range("bytes=0-29"), body().await, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(54), content_length);
                assert_eq!(ContentRange::bytes(0..30, 54).unwrap(), content_range);
                assert_eq!("Hello world this is a file to ", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_partial_response_2() {
        let ranged = Ranged::new(range("bytes=30-53"), body().await, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                println!("content-type: {:?}", content_type);
                assert_eq!(ContentLength(54), content_length);
                assert_eq!(ContentRange::bytes(30..54, 54).unwrap(), content_range);
                assert_eq!("test range requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_unbounded_start_response() {
        // unbounded ranges in HTTP are actually a suffix

        let ranged = Ranged::new(range("bytes=-20"), body().await, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(54), content_length);
                assert_eq!(ContentRange::bytes(34..54, 54).unwrap(), content_range);
                assert_eq!(" range requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_unbounded_end_response() {
        let ranged = Ranged::new(range("bytes=40-"), body().await, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(54), content_length);
                assert_eq!(ContentRange::bytes(40..54, 54).unwrap(), content_range);
                assert_eq!(" requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_one_byte_response() {
        let ranged = Ranged::new(range("bytes=30-30"), body().await, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(54), content_length);
                assert_eq!(ContentRange::bytes(30..31, 54).unwrap(), content_range);
                assert_eq!("t", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_invalid_range() {
        let ranged = Ranged::new(range("bytes=30-29"), body().await, None);

        let err = ranged.try_respond().err().expect("try_respond should return Err");

        let expected_content_range = ContentRange::unsatisfied_bytes(54);
        assert_eq!(expected_content_range, err.0)
    }

    #[tokio::test]
    async fn test_range_end_exceed_length() {
        let ranged = Ranged::new(range("bytes=30-99"), body().await, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(54), content_length);
                assert_eq!(ContentRange::bytes(30..54, 54).unwrap(), content_range);
                assert_eq!("test range requests on!\n", &collect_stream(stream).await);
            }
            _ => panic!("Expected a single range response"),
        }
    }

    #[tokio::test]
    async fn test_range_start_exceed_length() {
        let ranged = Ranged::new(range("bytes=99-"), body().await, None);

        let err = ranged.try_respond().err().expect("try_respond should return Err");

        let expected_content_range = ContentRange::unsatisfied_bytes(54);
        assert_eq!(expected_content_range, err.0)
    }
}

#[cfg(test)]
mod rfc_compliance_tests {
    use std::io;
    use std::path::PathBuf;
    use std::str::FromStr;
    use bytes::Bytes;
    use futures::{pin_mut, Stream, StreamExt};
    use axum::http::{HeaderValue, StatusCode};
    use axum_extra::headers::{ContentLength, ContentRange, Header, Range};
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::{Ranged, KnownSize, RangedResponse, RangeNotSatisfiable, stream};

    // Utility functions for tests
    async fn collect_stream(stream: impl Stream<Item=io::Result<Bytes>>) -> String {
        let mut string = String::new();
        pin_mut!(stream);
        while let Some(chunk) = stream.next().await.transpose().unwrap() {
            string += std::str::from_utf8(&chunk).unwrap();
        }
        string
    }

    async fn collect_multipart_stream(stream: impl Stream<Item=io::Result<Bytes>>, boundary: &str) -> String {
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

    // sleep for a random amount of time to ensure the file is created before reading
    async fn body(path: &str) -> KnownSize<File> {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Open the file and wrap it in KnownSize
        let file = File::open(path).await.unwrap();
        let file = PathBuf::from_str(path).unwrap();
        KnownSize::file(file).await.unwrap()
    }

    async fn body_blocks(path: &str, block_len: u64) -> KnownSize<File> {
        // sleep for a random amount of time to ensure the file is created before reading
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Open the file and wrap it in KnownSize with specified block length
        // let file = File::open(path).await.unwrap();
        KnownSize::file_blocks(path, block_len).await.unwrap()
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
        let ranged = Ranged::new(None, body(&test_path).await, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Full { content_length, stream, content_type } => {
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
        let ranged = Ranged::new(range("bytes=0-9"), body(&test_path).await, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
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
        let ranged = Ranged::new(range("bytes=50-999"), body(&test_path).await, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
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
        let ranged = Ranged::new(range("bytes=100-"), body(&test_path).await, None);
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
        let ranged = Ranged::new(range("bytes=-10"), body(&test_path).await, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
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
        let ranged = Ranged::new(range("bytes=-100"), body, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
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
        let ranged = Ranged::new(range("bytes=30-20"), body(&test_path).await, None);
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
        let ranged = Ranged::new(range("bytes=0-0,-1"), body(&test_path).await, None);

        println!("Testing first and last byte range : ranged={:?}", ranged);

        // Since our implementation doesn't support multiple ranges yet,
        // it should only return the first range
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Multiple { stream, boundary, content_type, content_length } => {
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
        let ranged = Ranged::new(range("bytes=0-9,20-29,40-49"), body, None);

        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Multiple { stream, boundary, content_type, content_length } => {
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
        let ranged = Ranged::new(range("bytes=10-9"), body(&test_path).await, None);
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
        let ranged = Ranged::new(range("bytes=50-"), body(&test_path).await, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
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
        let ranged = Ranged::new(None, body, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Full { content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
                let content = collect_stream(stream).await;
                assert_eq!("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", &content);
            }
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                panic!("Expected a full response, got single range response");
            }
            RangedResponse::Multiple { stream, boundary, content_type, content_length } => {
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
        let ranged = Ranged::new(range("bytes=0-29"), body, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
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
        let ranged = Ranged::new(range("bytes=10-29"), body, None);
        let response = ranged.try_respond().expect("try_respond should return Ok");

        match response {
            RangedResponse::Single { content_range, content_length, stream, content_type } => {
                assert_eq!(ContentLength(62), content_length);
                assert_eq!(ContentRange::bytes(10..20, 62).unwrap(), content_range);
                let content = collect_stream(stream).await;
                assert_eq!("ABCDEFGHIJ", &content);
            }
            _ => panic!("Expected a single range response"),
        }

        Ok(())
    }

    mod examples_from_rfc7233 {
        use super::*;

        #[tokio::test]
        async fn test_first_500_bytes() {
            let file_length = std::fs::metadata("Cargo.lock").unwrap().len();
            let body = body("Cargo.lock").await;
            let ranged = Ranged::new(range("bytes=0-499"), body, None);
            let response = ranged.try_respond().expect("try_respond should return Ok");
            match response {
                RangedResponse::Single { content_range, content_length, stream, content_type } => {
                    assert_eq!(ContentLength(file_length), content_length);
                    assert_eq!(ContentRange::bytes(0..500, file_length).unwrap(), content_range);
                    let content = collect_stream(stream).await;
                    // Here you would check the first 500 bytes of the file
                    // assert_eq!(expected_content, &content);
                }
                _ => panic!("Expected a single range response: {:?}", response),
            }
        }

        /*
           Examples of byte-ranges-specifier values:

           o  The first 500 bytes (byte offsets 0-499, inclusive):

                bytes=0-499

           o  The second 500 bytes (byte offsets 500-999, inclusive):

                bytes=500-999

           A client can request the last N bytes of the selected representation
           using a suffix-byte-range-spec.

             suffix-byte-range-spec = "-" suffix-length
             suffix-length = 1*DIGIT

           If the selected representation is shorter than the specified
           suffix-length, the entire representation is used.

           Additional examples, assuming a representation of length 10000:

           o  The final 500 bytes (byte offsets 9500-9999, inclusive):

                bytes=-500

           Or:

                bytes=9500-

           o  The first and last bytes only (bytes 0 and 9999):

                bytes=0-0,-1

           o  Other valid (but not canonical) specifications of the second 500
              bytes (byte offsets 500-999, inclusive):

                bytes=500-600,601-999
                bytes=500-700,601-999

         */
    }
}

/*

Internet Engineering Task Force (IETF)                  R. Fielding, Ed.
Request for Comments: 7233                                         Adobe
Obsoletes: 2616                                            Y. Lafon, Ed.
Category: Standards Track                                            W3C
ISSN: 2070-1721                                          J. Reschke, Ed.
                                                              greenbytes
                                                              June 2014


         Hypertext Transfer Protocol (HTTP/1.1): Range Requests

Abstract

   The Hypertext Transfer Protocol (HTTP) is a stateless application-
   level protocol for distributed, collaborative, hypertext information
   systems.  This document defines range requests and the rules for
   constructing and combining responses to those requests.

Status of This Memo

   This is an Internet Standards Track document.

   This document is a product of the Internet Engineering Task Force
   (IETF).  It represents the consensus of the IETF community.  It has
   received public review and has been approved for publication by the
   Internet Engineering Steering Group (IESG).  Further information on
   Internet Standards is available in Section 2 of RFC 5741.

   Information about the current status of this document, any errata,
   and how to provide feedback on it may be obtained at
   http://www.rfc-editor.org/info/rfc7233.




















Fielding, et al.             Standards Track                    [Page 1]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


Copyright Notice

   Copyright (c) 2014 IETF Trust and the persons identified as the
   document authors.  All rights reserved.

   This document is subject to BCP 78 and the IETF Trust's Legal
   Provisions Relating to IETF Documents
   (http://trustee.ietf.org/license-info) in effect on the date of
   publication of this document.  Please review these documents
   carefully, as they describe your rights and restrictions with respect
   to this document.  Code Components extracted from this document must
   include Simplified BSD License text as described in Section 4.e of
   the Trust Legal Provisions and are provided without warranty as
   described in the Simplified BSD License.

   This document may contain material from IETF Documents or IETF
   Contributions published or made publicly available before November
   10, 2008.  The person(s) controlling the copyright in some of this
   material may not have granted the IETF Trust the right to allow
   modifications of such material outside the IETF Standards Process.
   Without obtaining an adequate license from the person(s) controlling
   the copyright in such materials, this document may not be modified
   outside the IETF Standards Process, and derivative works of it may
   not be created outside the IETF Standards Process, except to format
   it for publication as an RFC or to translate it into languages other
   than English.

























Fielding, et al.             Standards Track                    [Page 2]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


Table of Contents

   1. Introduction ....................................................4
      1.1. Conformance and Error Handling .............................4
      1.2. Syntax Notation ............................................4
   2. Range Units .....................................................5
      2.1. Byte Ranges ................................................5
      2.2. Other Range Units ..........................................7
      2.3. Accept-Ranges ..............................................7
   3. Range Requests ..................................................8
      3.1. Range ......................................................8
      3.2. If-Range ...................................................9
   4. Responses to a Range Request ...................................10
      4.1. 206 Partial Content .......................................10
      4.2. Content-Range .............................................12
      4.3. Combining Ranges ..........................................14
      4.4. 416 Range Not Satisfiable .................................15
   5. IANA Considerations ............................................16
      5.1. Range Unit Registry .......................................16
           5.1.1. Procedure ..........................................16
           5.1.2. Registrations ......................................16
      5.2. Status Code Registration ..................................17
      5.3. Header Field Registration .................................17
      5.4. Internet Media Type Registration ..........................17
           5.4.1. Internet Media Type multipart/byteranges ...........18
   6. Security Considerations ........................................19
      6.1. Denial-of-Service Attacks Using Range .....................19
   7. Acknowledgments ................................................19
   8. References .....................................................20
      8.1. Normative References ......................................20
      8.2. Informative References ....................................20
   Appendix A. Internet Media Type multipart/byteranges ..............21
   Appendix B. Changes from RFC 2616 .................................22
   Appendix C. Imported ABNF .........................................22
   Appendix D. Collected ABNF ........................................23
   Index .............................................................24















Fielding, et al.             Standards Track                    [Page 3]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


1.  Introduction

   Hypertext Transfer Protocol (HTTP) clients often encounter
   interrupted data transfers as a result of canceled requests or
   dropped connections.  When a client has stored a partial
   representation, it is desirable to request the remainder of that
   representation in a subsequent request rather than transfer the
   entire representation.  Likewise, devices with limited local storage
   might benefit from being able to request only a subset of a larger
   representation, such as a single page of a very large document, or
   the dimensions of an embedded image.

   This document defines HTTP/1.1 range requests, partial responses, and
   the multipart/byteranges media type.  Range requests are an OPTIONAL
   feature of HTTP, designed so that recipients not implementing this
   feature (or not supporting it for the target resource) can respond as
   if it is a normal GET request without impacting interoperability.
   Partial responses are indicated by a distinct status code to not be
   mistaken for full responses by caches that might not implement the
   feature.

   Although the range request mechanism is designed to allow for
   extensible range types, this specification only defines requests for
   byte ranges.

1.1.  Conformance and Error Handling

   The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
   "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
   document are to be interpreted as described in [RFC2119].

   Conformance criteria and considerations regarding error handling are
   defined in Section 2.5 of [RFC7230].

1.2.  Syntax Notation

   This specification uses the Augmented Backus-Naur Form (ABNF)
   notation of [RFC5234] with a list extension, defined in Section 7 of
   [RFC7230], that allows for compact definition of comma-separated
   lists using a '#' operator (similar to how the '*' operator indicates
   repetition).  Appendix C describes rules imported from other
   documents.  Appendix D shows the collected grammar with all list
   operators expanded to standard ABNF notation.








Fielding, et al.             Standards Track                    [Page 4]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


2.  Range Units

   A representation can be partitioned into subranges according to
   various structural units, depending on the structure inherent in the
   representation's media type.  This "range unit" is used in the
   Accept-Ranges (Section 2.3) response header field to advertise
   support for range requests, the Range (Section 3.1) request header
   field to delineate the parts of a representation that are requested,
   and the Content-Range (Section 4.2) payload header field to describe
   which part of a representation is being transferred.

     range-unit       = bytes-unit / other-range-unit

2.1.  Byte Ranges

   Since representation data is transferred in payloads as a sequence of
   octets, a byte range is a meaningful substructure for any
   representation transferable over HTTP (Section 3 of [RFC7231]).  The
   "bytes" range unit is defined for expressing subranges of the data's
   octet sequence.

     bytes-unit       = "bytes"

   A byte-range request can specify a single range of bytes or a set of
   ranges within a single representation.

     byte-ranges-specifier = bytes-unit "=" byte-range-set
     byte-range-set  = 1#( byte-range-spec / suffix-byte-range-spec )
     byte-range-spec = first-byte-pos "-" [ last-byte-pos ]
     first-byte-pos  = 1*DIGIT
     last-byte-pos   = 1*DIGIT

   The first-byte-pos value in a byte-range-spec gives the byte-offset
   of the first byte in a range.  The last-byte-pos value gives the
   byte-offset of the last byte in the range; that is, the byte
   positions specified are inclusive.  Byte offsets start at zero.

   Examples of byte-ranges-specifier values:

   o  The first 500 bytes (byte offsets 0-499, inclusive):

        bytes=0-499

   o  The second 500 bytes (byte offsets 500-999, inclusive):

        bytes=500-999





Fielding, et al.             Standards Track                    [Page 5]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   A byte-range-spec is invalid if the last-byte-pos value is present
   and less than the first-byte-pos.

   A client can limit the number of bytes requested without knowing the
   size of the selected representation.  If the last-byte-pos value is
   absent, or if the value is greater than or equal to the current
   length of the representation data, the byte range is interpreted as
   the remainder of the representation (i.e., the server replaces the
   value of last-byte-pos with a value that is one less than the current
   length of the selected representation).

   A client can request the last N bytes of the selected representation
   using a suffix-byte-range-spec.

     suffix-byte-range-spec = "-" suffix-length
     suffix-length = 1*DIGIT

   If the selected representation is shorter than the specified
   suffix-length, the entire representation is used.

   Additional examples, assuming a representation of length 10000:

   o  The final 500 bytes (byte offsets 9500-9999, inclusive):

        bytes=-500

   Or:

        bytes=9500-

   o  The first and last bytes only (bytes 0 and 9999):

        bytes=0-0,-1

   o  Other valid (but not canonical) specifications of the second 500
      bytes (byte offsets 500-999, inclusive):

        bytes=500-600,601-999
        bytes=500-700,601-999

   If a valid byte-range-set includes at least one byte-range-spec with
   a first-byte-pos that is less than the current length of the
   representation, or at least one suffix-byte-range-spec with a
   non-zero suffix-length, then the byte-range-set is satisfiable.
   Otherwise, the byte-range-set is unsatisfiable.






Fielding, et al.             Standards Track                    [Page 6]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   In the byte-range syntax, first-byte-pos, last-byte-pos, and
   suffix-length are expressed as decimal number of octets.  Since there
   is no predefined limit to the length of a payload, recipients MUST
   anticipate potentially large decimal numerals and prevent parsing
   errors due to integer conversion overflows.

2.2.  Other Range Units

   Range units are intended to be extensible.  New range units ought to
   be registered with IANA, as defined in Section 5.1.

     other-range-unit = token

2.3.  Accept-Ranges

   The "Accept-Ranges" header field allows a server to indicate that it
   supports range requests for the target resource.

     Accept-Ranges     = acceptable-ranges
     acceptable-ranges = 1#range-unit / "none"

   An origin server that supports byte-range requests for a given target
   resource MAY send

     Accept-Ranges: bytes

   to indicate what range units are supported.  A client MAY generate
   range requests without having received this header field for the
   resource involved.  Range units are defined in Section 2.

   A server that does not support any kind of range request for the
   target resource MAY send

     Accept-Ranges: none

   to advise the client not to attempt a range request.















Fielding, et al.             Standards Track                    [Page 7]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


3.  Range Requests

3.1.  Range

   The "Range" header field on a GET request modifies the method
   semantics to request transfer of only one or more subranges of the
   selected representation data, rather than the entire selected
   representation data.

     Range = byte-ranges-specifier / other-ranges-specifier
     other-ranges-specifier = other-range-unit "=" other-range-set
     other-range-set = 1*VCHAR

   A server MAY ignore the Range header field.  However, origin servers
   and intermediate caches ought to support byte ranges when possible,
   since Range supports efficient recovery from partially failed
   transfers and partial retrieval of large representations.  A server
   MUST ignore a Range header field received with a request method other
   than GET.

   An origin server MUST ignore a Range header field that contains a
   range unit it does not understand.  A proxy MAY discard a Range
   header field that contains a range unit it does not understand.

   A server that supports range requests MAY ignore or reject a Range
   header field that consists of more than two overlapping ranges, or a
   set of many small ranges that are not listed in ascending order,
   since both are indications of either a broken client or a deliberate
   denial-of-service attack (Section 6.1).  A client SHOULD NOT request
   multiple ranges that are inherently less efficient to process and
   transfer than a single range that encompasses the same data.

   A client that is requesting multiple ranges SHOULD list those ranges
   in ascending order (the order in which they would typically be
   received in a complete representation) unless there is a specific
   need to request a later part earlier.  For example, a user agent
   processing a large representation with an internal catalog of parts
   might need to request later parts first, particularly if the
   representation consists of pages stored in reverse order and the user
   agent wishes to transfer one page at a time.

   The Range header field is evaluated after evaluating the precondition
   header fields defined in [RFC7232], and only if the result in absence
   of the Range header field would be a 200 (OK) response.  In other
   words, Range is ignored when a conditional GET would result in a 304
   (Not Modified) response.





Fielding, et al.             Standards Track                    [Page 8]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   The If-Range header field (Section 3.2) can be used as a precondition
   to applying the Range header field.

   If all of the preconditions are true, the server supports the Range
   header field for the target resource, and the specified range(s) are
   valid and satisfiable (as defined in Section 2.1), the server SHOULD
   send a 206 (Partial Content) response with a payload containing one
   or more partial representations that correspond to the satisfiable
   ranges requested, as defined in Section 4.

   If all of the preconditions are true, the server supports the Range
   header field for the target resource, and the specified range(s) are
   invalid or unsatisfiable, the server SHOULD send a 416 (Range Not
   Satisfiable) response.

3.2.  If-Range

   If a client has a partial copy of a representation and wishes to have
   an up-to-date copy of the entire representation, it could use the
   Range header field with a conditional GET (using either or both of
   If-Unmodified-Since and If-Match.)  However, if the precondition
   fails because the representation has been modified, the client would
   then have to make a second request to obtain the entire current
   representation.

   The "If-Range" header field allows a client to "short-circuit" the
   second request.  Informally, its meaning is as follows: if the
   representation is unchanged, send me the part(s) that I am requesting
   in Range; otherwise, send me the entire representation.

     If-Range = entity-tag / HTTP-date

   A client MUST NOT generate an If-Range header field in a request that
   does not contain a Range header field.  A server MUST ignore an
   If-Range header field received in a request that does not contain a
   Range header field.  An origin server MUST ignore an If-Range header
   field received in a request for a target resource that does not
   support Range requests.

   A client MUST NOT generate an If-Range header field containing an
   entity-tag that is marked as weak.  A client MUST NOT generate an
   If-Range header field containing an HTTP-date unless the client has
   no entity-tag for the corresponding representation and the date is a
   strong validator in the sense defined by Section 2.2.2 of [RFC7232].

   A server that evaluates an If-Range precondition MUST use the strong
   comparison function when comparing entity-tags (Section 2.3.2 of
   [RFC7232]) and MUST evaluate the condition as false if an HTTP-date



Fielding, et al.             Standards Track                    [Page 9]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   validator is provided that is not a strong validator in the sense
   defined by Section 2.2.2 of [RFC7232].  A valid entity-tag can be
   distinguished from a valid HTTP-date by examining the first two
   characters for a DQUOTE.

   If the validator given in the If-Range header field matches the
   current validator for the selected representation of the target
   resource, then the server SHOULD process the Range header field as
   requested.  If the validator does not match, the server MUST ignore
   the Range header field.  Note that this comparison by exact match,
   including when the validator is an HTTP-date, differs from the
   "earlier than or equal to" comparison used when evaluating an
   If-Unmodified-Since conditional.

4.  Responses to a Range Request

4.1.  206 Partial Content

   The 206 (Partial Content) status code indicates that the server is
   successfully fulfilling a range request for the target resource by
   transferring one or more parts of the selected representation that
   correspond to the satisfiable ranges found in the request's Range
   header field (Section 3.1).

   If a single part is being transferred, the server generating the 206
   response MUST generate a Content-Range header field, describing what
   range of the selected representation is enclosed, and a payload
   consisting of the range.  For example:

     HTTP/1.1 206 Partial Content
     Date: Wed, 15 Nov 1995 06:25:24 GMT
     Last-Modified: Wed, 15 Nov 1995 04:58:08 GMT
     Content-Range: bytes 21010-47021/47022
     Content-Length: 26012
     Content-Type: image/gif

     ... 26012 bytes of partial image data ...

   If multiple parts are being transferred, the server generating the
   206 response MUST generate a "multipart/byteranges" payload, as
   defined in Appendix A, and a Content-Type header field containing the
   multipart/byteranges media type and its required boundary parameter.
   To avoid confusion with single-part responses, a server MUST NOT
   generate a Content-Range header field in the HTTP header section of a
   multiple part response (this field will be sent in each part
   instead).





Fielding, et al.             Standards Track                   [Page 10]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   Within the header area of each body part in the multipart payload,
   the server MUST generate a Content-Range header field corresponding
   to the range being enclosed in that body part.  If the selected
   representation would have had a Content-Type header field in a 200
   (OK) response, the server SHOULD generate that same Content-Type
   field in the header area of each body part.  For example:

     HTTP/1.1 206 Partial Content
     Date: Wed, 15 Nov 1995 06:25:24 GMT
     Last-Modified: Wed, 15 Nov 1995 04:58:08 GMT
     Content-Length: 1741
     Content-Type: multipart/byteranges; boundary=THIS_STRING_SEPARATES

     --THIS_STRING_SEPARATES
     Content-Type: application/pdf
     Content-Range: bytes 500-999/8000

     ...the first range...
     --THIS_STRING_SEPARATES
     Content-Type: application/pdf
     Content-Range: bytes 7000-7999/8000

     ...the second range
     --THIS_STRING_SEPARATES--

   When multiple ranges are requested, a server MAY coalesce any of the
   ranges that overlap, or that are separated by a gap that is smaller
   than the overhead of sending multiple parts, regardless of the order
   in which the corresponding byte-range-spec appeared in the received
   Range header field.  Since the typical overhead between parts of a
   multipart/byteranges payload is around 80 bytes, depending on the
   selected representation's media type and the chosen boundary
   parameter length, it can be less efficient to transfer many small
   disjoint parts than it is to transfer the entire selected
   representation.

   A server MUST NOT generate a multipart response to a request for a
   single range, since a client that does not request multiple parts
   might not support multipart responses.  However, a server MAY
   generate a multipart/byteranges payload with only a single body part
   if multiple ranges were requested and only one range was found to be
   satisfiable or only one range remained after coalescing.  A client
   that cannot process a multipart/byteranges response MUST NOT generate
   a request that asks for multiple ranges.

   When a multipart response payload is generated, the server SHOULD
   send the parts in the same order that the corresponding
   byte-range-spec appeared in the received Range header field,



Fielding, et al.             Standards Track                   [Page 11]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   excluding those ranges that were deemed unsatisfiable or that were
   coalesced into other ranges.  A client that receives a multipart
   response MUST inspect the Content-Range header field present in each
   body part in order to determine which range is contained in that body
   part; a client cannot rely on receiving the same ranges that it
   requested, nor the same order that it requested.

   When a 206 response is generated, the server MUST generate the
   following header fields, in addition to those required above, if the
   field would have been sent in a 200 (OK) response to the same
   request: Date, Cache-Control, ETag, Expires, Content-Location, and
   Vary.

   If a 206 is generated in response to a request with an If-Range
   header field, the sender SHOULD NOT generate other representation
   header fields beyond those required above, because the client is
   understood to already have a prior response containing those header
   fields.  Otherwise, the sender MUST generate all of the
   representation header fields that would have been sent in a 200 (OK)
   response to the same request.

   A 206 response is cacheable by default; i.e., unless otherwise
   indicated by explicit cache controls (see Section 4.2.2 of
   [RFC7234]).

4.2.  Content-Range

   The "Content-Range" header field is sent in a single part 206
   (Partial Content) response to indicate the partial range of the
   selected representation enclosed as the message payload, sent in each
   part of a multipart 206 response to indicate the range enclosed
   within each body part, and sent in 416 (Range Not Satisfiable)
   responses to provide information about the selected representation.

     Content-Range       = byte-content-range
                         / other-content-range

     byte-content-range  = bytes-unit SP
                           ( byte-range-resp / unsatisfied-range )

     byte-range-resp     = byte-range "/" ( complete-length / "*" )
     byte-range          = first-byte-pos "-" last-byte-pos
     unsatisfied-range   = "* /" complete-length

     complete-length     = 1*DIGIT

     other-content-range = other-range-unit SP other-range-resp
     other-range-resp    = *CHAR



Fielding, et al.             Standards Track                   [Page 12]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   If a 206 (Partial Content) response contains a Content-Range header
   field with a range unit (Section 2) that the recipient does not
   understand, the recipient MUST NOT attempt to recombine it with a
   stored representation.  A proxy that receives such a message SHOULD
   forward it downstream.

   For byte ranges, a sender SHOULD indicate the complete length of the
   representation from which the range has been extracted, unless the
   complete length is unknown or difficult to determine.  An asterisk
   character ("*") in place of the complete-length indicates that the
   representation length was unknown when the header field was
   generated.

   The following example illustrates when the complete length of the
   selected representation is known by the sender to be 1234 bytes:

     Content-Range: bytes 42-1233/1234

   and this second example illustrates when the complete length is
   unknown:

     Content-Range: bytes 42-1233/*

   A Content-Range field value is invalid if it contains a
   byte-range-resp that has a last-byte-pos value less than its
   first-byte-pos value, or a complete-length value less than or equal
   to its last-byte-pos value.  The recipient of an invalid
   Content-Range MUST NOT attempt to recombine the received content with
   a stored representation.

   A server generating a 416 (Range Not Satisfiable) response to a
   byte-range request SHOULD send a Content-Range header field with an
   unsatisfied-range value, as in the following example:

     Content-Range: bytes */1234

   The complete-length in a 416 response indicates the current length of
   the selected representation.

   The Content-Range header field has no meaning for status codes that
   do not explicitly describe its semantic.  For this specification,
   only the 206 (Partial Content) and 416 (Range Not Satisfiable) status
   codes describe a meaning for Content-Range.








Fielding, et al.             Standards Track                   [Page 13]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   The following are examples of Content-Range values in which the
   selected representation contains a total of 1234 bytes:

   o  The first 500 bytes:

        Content-Range: bytes 0-499/1234

   o  The second 500 bytes:

        Content-Range: bytes 500-999/1234

   o  All except for the first 500 bytes:

        Content-Range: bytes 500-1233/1234

   o  The last 500 bytes:

        Content-Range: bytes 734-1233/1234

4.3.  Combining Ranges

   A response might transfer only a subrange of a representation if the
   connection closed prematurely or if the request used one or more
   Range specifications.  After several such transfers, a client might
   have received several ranges of the same representation.  These
   ranges can only be safely combined if they all have in common the
   same strong validator (Section 2.1 of [RFC7232]).

   A client that has received multiple partial responses to GET requests
   on a target resource MAY combine those responses into a larger
   continuous range if they share the same strong validator.

   If the most recent response is an incomplete 200 (OK) response, then
   the header fields of that response are used for any combined response
   and replace those of the matching stored responses.

   If the most recent response is a 206 (Partial Content) response and
   at least one of the matching stored responses is a 200 (OK), then the
   combined response header fields consist of the most recent 200
   response's header fields.  If all of the matching stored responses
   are 206 responses, then the stored response with the most recent
   header fields is used as the source of header fields for the combined
   response, except that the client MUST use other header fields
   provided in the new response, aside from Content-Range, to replace
   all instances of the corresponding header fields in the stored
   response.





Fielding, et al.             Standards Track                   [Page 14]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   The combined response message body consists of the union of partial
   content ranges in the new response and each of the selected
   responses.  If the union consists of the entire range of the
   representation, then the client MUST process the combined response as
   if it were a complete 200 (OK) response, including a Content-Length
   header field that reflects the complete length.  Otherwise, the
   client MUST process the set of continuous ranges as one of the
   following: an incomplete 200 (OK) response if the combined response
   is a prefix of the representation, a single 206 (Partial Content)
   response containing a multipart/byteranges body, or multiple 206
   (Partial Content) responses, each with one continuous range that is
   indicated by a Content-Range header field.

4.4.  416 Range Not Satisfiable

   The 416 (Range Not Satisfiable) status code indicates that none of
   the ranges in the request's Range header field (Section 3.1) overlap
   the current extent of the selected resource or that the set of ranges
   requested has been rejected due to invalid ranges or an excessive
   request of small or overlapping ranges.

   For byte ranges, failing to overlap the current extent means that the
   first-byte-pos of all of the byte-range-spec values were greater than
   the current length of the selected representation.  When this status
   code is generated in response to a byte-range request, the sender
   SHOULD generate a Content-Range header field specifying the current
   length of the selected representation (Section 4.2).

   For example:

     HTTP/1.1 416 Range Not Satisfiable
     Date: Fri, 20 Jan 2012 15:41:54 GMT
     Content-Range: bytes * /47022

      Note: Because servers are free to ignore Range, many
      implementations will simply respond with the entire selected
      representation in a 200 (OK) response.  That is partly because
      most clients are prepared to receive a 200 (OK) to complete the
      task (albeit less efficiently) and partly because clients might
      not stop making an invalid partial request until they have
      received a complete representation.  Thus, clients cannot depend
      on receiving a 416 (Range Not Satisfiable) response even when it
      is most appropriate.








Fielding, et al.             Standards Track                   [Page 15]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


5.  IANA Considerations

5.1.  Range Unit Registry

   The "HTTP Range Unit Registry" defines the namespace for the range
   unit names and refers to their corresponding specifications.  The
   registry has been created and is now maintained at
   <http://www.iana.org/assignments/http-parameters>.

5.1.1.  Procedure

   Registration of an HTTP Range Unit MUST include the following fields:

   o  Name

   o  Description

   o  Pointer to specification text

   Values to be added to this namespace require IETF Review (see
   [RFC5226], Section 4.1).

5.1.2.  Registrations

   The initial range unit registry contains the registrations below:

   +-------------+---------------------------------------+-------------+
   | Range Unit  | Description                           | Reference   |
   | Name        |                                       |             |
   +-------------+---------------------------------------+-------------+
   | bytes       | a range of octets                     | Section 2.1 |
   | none        | reserved as keyword, indicating no    | Section 2.3 |
   |             | ranges are supported                  |             |
   +-------------+---------------------------------------+-------------+

   The change controller is: "IETF (iesg@ietf.org) - Internet
    Engineering Task Force".














Fielding, et al.             Standards Track                   [Page 16]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


5.2.  Status Code Registration

   The "Hypertext Transfer Protocol (HTTP) Status Code Registry" located
   at <http://www.iana.org/assignments/http-status-codes> has been
   updated to include the registrations below:

   +-------+-----------------------+-------------+
   | Value | Description           | Reference   |
   +-------+-----------------------+-------------+
   | 206   | Partial Content       | Section 4.1 |
   | 416   | Range Not Satisfiable | Section 4.4 |
   +-------+-----------------------+-------------+

5.3.  Header Field Registration

   HTTP header fields are registered within the "Message Headers"
   registry maintained at
   <http://www.iana.org/assignments/message-headers/>.

   This document defines the following HTTP header fields, so their
   associated registry entries have been updated according to the
   permanent registrations below (see [BCP90]):

   +-------------------+----------+----------+-------------+
   | Header Field Name | Protocol | Status   | Reference   |
   +-------------------+----------+----------+-------------+
   | Accept-Ranges     | http     | standard | Section 2.3 |
   | Content-Range     | http     | standard | Section 4.2 |
   | If-Range          | http     | standard | Section 3.2 |
   | Range             | http     | standard | Section 3.1 |
   +-------------------+----------+----------+-------------+

   The change controller is: "IETF (iesg@ietf.org) - Internet
    Engineering Task Force".

5.4.  Internet Media Type Registration

   IANA maintains the registry of Internet media types [BCP13] at
   <http://www.iana.org/assignments/media-types>.

   This document serves as the specification for the Internet media type
   "multipart/byteranges".  The following has been registered with IANA.









Fielding, et al.             Standards Track                   [Page 17]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


5.4.1.  Internet Media Type multipart/byteranges

   Type name:  multipart

   Subtype name:  byteranges

   Required parameters:  boundary

   Optional parameters:  N/A

   Encoding considerations:  only "7bit", "8bit", or "binary" are
      permitted

   Security considerations:  see Section 6

   Interoperability considerations:  N/A

   Published specification:  This specification (see Appendix A).

   Applications that use this media type:  HTTP components supporting
      multiple ranges in a single request.

   Fragment identifier considerations:  N/A

   Additional information:

      Deprecated alias names for this type:  N/A

      Magic number(s):  N/A

      File extension(s):  N/A

      Macintosh file type code(s):  N/A

   Person and email address to contact for further information:  See
      Authors' Addresses section.

   Intended usage:  COMMON

   Restrictions on usage:  N/A

   Author:  See Authors' Addresses section.

   Change controller:  IESG







Fielding, et al.             Standards Track                   [Page 18]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


6.  Security Considerations

   This section is meant to inform developers, information providers,
   and users of known security concerns specific to the HTTP range
   request mechanisms.  More general security considerations are
   addressed in HTTP messaging [RFC7230] and semantics [RFC7231].

6.1.  Denial-of-Service Attacks Using Range

   Unconstrained multiple range requests are susceptible to denial-of-
   service attacks because the effort required to request many
   overlapping ranges of the same data is tiny compared to the time,
   memory, and bandwidth consumed by attempting to serve the requested
   data in many parts.  Servers ought to ignore, coalesce, or reject
   egregious range requests, such as requests for more than two
   overlapping ranges or for many small ranges in a single set,
   particularly when the ranges are requested out of order for no
   apparent reason.  Multipart range requests are not designed to
   support random access.

7.  Acknowledgments

   See Section 10 of [RFC7230].




























Fielding, et al.             Standards Track                   [Page 19]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


8.  References

8.1.  Normative References

   [RFC2046]  Freed, N. and N. Borenstein, "Multipurpose Internet Mail
    Extensions (MIME) Part Two: Media Types", RFC 2046,
              November 1996.

   [RFC2119]  Bradner, S., "Key words for use in RFCs to Indicate
    Requirement Levels", BCP 14, RFC 2119, March 1997.

   [RFC5234]  Crocker, D., Ed. and P. Overell, "Augmented BNF for Syntax
    Specifications: ABNF", STD 68, RFC 5234, January 2008.

   [RFC7230]  Fielding, R., Ed. and J. Reschke, Ed., "Hypertext Transfer
    Protocol (HTTP/1.1): Message Syntax and Routing",
              RFC 7230, June 2014.

   [RFC7231]  Fielding, R., Ed. and J. Reschke, Ed., "Hypertext Transfer
    Protocol (HTTP/1.1): Semantics and Content", RFC 7231,
              June 2014.

   [RFC7232]  Fielding, R., Ed. and J. Reschke, Ed., "Hypertext Transfer
    Protocol (HTTP/1.1): Conditional Requests", RFC 7232,
              June 2014.

   [RFC7234]  Fielding, R., Ed., Nottingham, M., Ed., and J. Reschke,
              Ed., "Hypertext Transfer Protocol (HTTP/1.1): Caching",
              RFC 7234, June 2014.

8.2.  Informative References

   [BCP13]    Freed, N., Klensin, J., and T. Hansen, "Media Type
    Specifications and Registration Procedures", BCP 13,
              RFC 6838, January 2013.

   [BCP90]    Klyne, G., Nottingham, M., and J. Mogul, "Registration
    Procedures for Message Header Fields", BCP 90, RFC 3864,
              September 2004.

   [RFC2616]  Fielding, R., Gettys, J., Mogul, J., Frystyk, H.,
              Masinter, L., Leach, P., and T. Berners-Lee, "Hypertext
    Transfer Protocol -- HTTP/1.1", RFC 2616, June 1999.

   [RFC5226]  Narten, T. and H. Alvestrand, "Guidelines for Writing an
    IANA Considerations Section in RFCs", BCP 26, RFC 5226,
              May 2008.




Fielding, et al.             Standards Track                   [Page 20]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


Appendix A.  Internet Media Type multipart/byteranges

   When a 206 (Partial Content) response message includes the content of
   multiple ranges, they are transmitted as body parts in a multipart
   message body ([RFC2046], Section 5.1) with the media type of
   "multipart/byteranges".

   The multipart/byteranges media type includes one or more body parts,
   each with its own Content-Type and Content-Range fields.  The
   required boundary parameter specifies the boundary string used to
   separate each body part.

   Implementation Notes:

   1.  Additional CRLFs might precede the first boundary string in the
       body.

   2.  Although [RFC2046] permits the boundary string to be quoted, some
       existing implementations handle a quoted boundary string
       incorrectly.

   3.  A number of clients and servers were coded to an early draft of
       the byteranges specification that used a media type of multipart/
       x-byteranges, which is almost (but not quite) compatible with
       this type.

   Despite the name, the "multipart/byteranges" media type is not
   limited to byte ranges.  The following example uses an "exampleunit"
   range unit:

     HTTP/1.1 206 Partial Content
     Date: Tue, 14 Nov 1995 06:25:24 GMT
     Last-Modified: Tue, 14 July 04:58:08 GMT
     Content-Length: 2331785
     Content-Type: multipart/byteranges; boundary=THIS_STRING_SEPARATES

     --THIS_STRING_SEPARATES
     Content-Type: video/example
     Content-Range: exampleunit 1.2-4.3/25

     ...the first range...
     --THIS_STRING_SEPARATES
     Content-Type: video/example
     Content-Range: exampleunit 11.2-14.3/25

     ...the second range
     --THIS_STRING_SEPARATES--




Fielding, et al.             Standards Track                   [Page 21]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


Appendix B.  Changes from RFC 2616

   Servers are given more leeway in how they respond to a range request,
   in order to mitigate abuse by malicious (or just greedy) clients.
   (Section 3.1)

   A weak validator cannot be used in a 206 response.  (Section 4.1)

   The Content-Range header field only has meaning when the status code
   explicitly defines its use.  (Section 4.2)

   This specification introduces a Range Unit Registry.  (Section 5.1)

   multipart/byteranges can consist of a single part.  (Appendix A)

Appendix C.  Imported ABNF

   The following core rules are included by reference, as defined in
   Appendix B.1 of [RFC5234]: ALPHA (letters), CR (carriage return),
   CRLF (CR LF), CTL (controls), DIGIT (decimal 0-9), DQUOTE (double
   quote), HEXDIG (hexadecimal 0-9/A-F/a-f), LF (line feed), OCTET (any
   8-bit sequence of data), SP (space), and VCHAR (any visible US-ASCII
   character).

   Note that all rules derived from token are to be compared
   case-insensitively, like range-unit and acceptable-ranges.

   The rules below are defined in [RFC7230]:

     OWS        = <OWS, see [RFC7230], Section 3.2.3>
     token      = <token, see [RFC7230], Section 3.2.6>

   The rules below are defined in other parts:

     HTTP-date  = <HTTP-date, see [RFC7231], Section 7.1.1.1>
     entity-tag = <entity-tag, see [RFC7232], Section 2.3>















Fielding, et al.             Standards Track                   [Page 22]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


Appendix D.  Collected ABNF

   In the collected ABNF below, list rules are expanded as per Section
   1.2 of [RFC7230].

   Accept-Ranges = acceptable-ranges

   Content-Range = byte-content-range / other-content-range

   HTTP-date = <HTTP-date, see [RFC7231], Section 7.1.1.1>

   If-Range = entity-tag / HTTP-date

   OWS = <OWS, see [RFC7230], Section 3.2.3>

   Range = byte-ranges-specifier / other-ranges-specifier

   acceptable-ranges = ( *( "," OWS ) range-unit *( OWS "," [ OWS
    range-unit ] ) ) / "none"

   byte-content-range = bytes-unit SP ( byte-range-resp /
    unsatisfied-range )
   byte-range = first-byte-pos "-" last-byte-pos
   byte-range-resp = byte-range "/" ( complete-length / "*" )
   byte-range-set = *( "," OWS ) ( byte-range-spec /
    suffix-byte-range-spec ) *( OWS "," [ OWS ( byte-range-spec /
    suffix-byte-range-spec ) ] )
   byte-range-spec = first-byte-pos "-" [ last-byte-pos ]
   byte-ranges-specifier = bytes-unit "=" byte-range-set
   bytes-unit = "bytes"

   complete-length = 1*DIGIT

   entity-tag = <entity-tag, see [RFC7232], Section 2.3>

   first-byte-pos = 1*DIGIT

   last-byte-pos = 1*DIGIT

   other-content-range = other-range-unit SP other-range-resp
   other-range-resp = *CHAR
   other-range-set = 1*VCHAR
   other-range-unit = token
   other-ranges-specifier = other-range-unit "=" other-range-set

   range-unit = bytes-unit / other-range-unit

   suffix-byte-range-spec = "-" suffix-length



Fielding, et al.             Standards Track                   [Page 23]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   suffix-length = 1*DIGIT

   token = <token, see [RFC7230], Section 3.2.6>

   unsatisfied-range = "* /" complete-length

Index

   2
      206 Partial Content (status code)  10

   4
      416 Range Not Satisfiable (status code)  15

   A
      Accept-Ranges header field  7

   C
      Content-Range header field  12

   G
      Grammar
         Accept-Ranges  7
         acceptable-ranges  7
         byte-content-range  12
         byte-range  12
         byte-range-resp  12
         byte-range-set  5
         byte-range-spec  5
         byte-ranges-specifier  5
         bytes-unit  5
         complete-length  12
         Content-Range  12
         first-byte-pos  5
         If-Range  9
         last-byte-pos  5
         other-content-range  12
         other-range-resp  12
         other-range-unit  5, 7
         Range  8
         range-unit  5
         ranges-specifier  5
         suffix-byte-range-spec  6
         suffix-length  6
         unsatisfied-range  12






Fielding, et al.             Standards Track                   [Page 24]

RFC 7233                 HTTP/1.1 Range Requests               June 2014


   I
      If-Range header field  9

   M
      Media Type
         multipart/byteranges  18, 21
         multipart/x-byteranges  19
      multipart/byteranges Media Type  18, 21
      multipart/x-byteranges Media Type  21

   R
      Range header field  8

Authors' Addresses

   Roy T. Fielding (editor)
   Adobe Systems Incorporated
   345 Park Ave
   San Jose, CA  95110
   USA

   EMail: fielding@gbiv.com
   URI:   http://roy.gbiv.com/


   Yves Lafon (editor)
   World Wide Web Consortium
   W3C / ERCIM
   2004, rte des Lucioles
   Sophia-Antipolis, AM  06902
   France

   EMail: ylafon@w3.org
   URI:   http://www.raubacapeu.net/people/yves/


   Julian F. Reschke (editor)
   greenbytes GmbH
   Hafenweg 16
   Muenster, NW  48155
   Germany

   EMail: julian.reschke@greenbytes.de
   URI:   http://greenbytes.de/tech/webdav/







Fielding, et al.             Standards Track                   [Page 25]



 */


