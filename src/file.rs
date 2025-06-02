use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project::pin_project;
use tokio::io::{ReadBuf, AsyncRead, AsyncSeek, AsyncSeekExt};

use crate::{RangeBody, AsyncSeekStart};

/// Implements [`RangeBody`] for any [`AsyncRead`] and [`AsyncSeekStart`], constructed with a fixed byte size.
#[pin_project]
pub struct KnownSize<B: AsyncRead + AsyncSeekStart> {
    byte_size: u64,
    block_len: u64,
    content_type: Option<String>,
    #[pin]
    body: B,
}

impl std::fmt::Debug for KnownSize<tokio::fs::File> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnownSize")
            .field("byte_size", &self.byte_size)
            .finish()
    }
}

impl KnownSize<tokio::fs::File> {
    /// Calls [`tokio::fs::File::metadata`] to determine file size.
    pub async fn file(path: PathBuf) -> io::Result<KnownSize<tokio::fs::File>> {
        let file = tokio::fs::File::open(&path).await?;
        let content_type = { // if cfg!(feature = "mime") {
            Some(path.extension().map(|ext| {
                match ext.to_str() {
                    Some(ext) => mime_guess::from_ext(ext).first_or_octet_stream().to_string(),
                    _ => "application/octet-stream".to_string(),
                }
            }).unwrap_or_else(|| "application/octet-stream".to_string()))
        };
        // Use metadata to get the file size.
        let byte_size = file.metadata().await?.len();
        let block_len = byte_size;
        Ok(KnownSize { byte_size, block_len, body: file, content_type })
    }

    /// Calls [`tokio::fs::File::metadata`] to determine file size.
    pub async fn file_blocks<P: AsRef<Path>>(path: P, block_len: u64) -> io::Result<KnownSize<tokio::fs::File>> {
        let path = path.as_ref();
        let file = tokio::fs::File::open(&path).await?;
        let byte_size = file.metadata().await?.len();
        let content_type = mime_guess::from_path(path);
        let content_type = Some(content_type.first_or_octet_stream().to_string());
        Ok(KnownSize { byte_size, block_len, body: file, content_type })
    }

    /// Returns the block length, which is the size of the body in
    /// bytes, or the maximum number of bytes to return in any one request.
    pub fn block_len(&self) -> u64 {
        self.block_len
    }
    
    pub fn content_type(&self) -> Option<String> {
        self.content_type.clone()
    }
    
}

impl<B: AsyncRead + AsyncSeekStart> KnownSize<B> {
    /// Construct a [`KnownSize`] instance with a byte size supplied manually.
    pub fn sized(body: B, byte_size: u64) -> Self {
        let block_len = byte_size;
        KnownSize { byte_size, block_len, body, content_type: None }
    }

    /// Construct a [`KnownSize`] instance with a byte size supplied manually.
    pub fn sized_blocks(body: B, byte_size: u64, block_len: u64) -> Self {
        KnownSize { byte_size, block_len, body, content_type: None }
    }
}

impl<B: AsyncRead + AsyncSeek + Unpin> KnownSize<B> {
    /// Uses `seek` to determine size by seeking to the end and getting stream position.
    pub async fn seek(mut body: B) -> io::Result<KnownSize<B>> {
        let byte_size = Pin::new(&mut body).seek(io::SeekFrom::End(0)).await?;
        let block_len = byte_size;
        Ok(KnownSize { byte_size, block_len, body, content_type: None })
    }

    /// Uses `seek` to determine size by seeking to the end and getting stream position.
    pub async fn seek_blocks(mut body: B, block_len: u64) -> io::Result<KnownSize<B>> {
        let byte_size = Pin::new(&mut body).seek(io::SeekFrom::End(0)).await?;
        Ok(KnownSize { byte_size, block_len, body, content_type: None })
    }
}

impl<B: AsyncRead + AsyncSeekStart> AsyncRead for KnownSize<B> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();
        this.body.poll_read(cx, buf)
    }
}

impl<B: AsyncRead + AsyncSeekStart> AsyncSeekStart for KnownSize<B> {
    fn start_seek(
        self: Pin<&mut Self>,
        position: u64,
    ) -> io::Result<()> {
        let this = self.project();
        this.body.start_seek(position)
    }

    fn poll_complete(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();
        this.body.poll_complete(cx)
    }
}

impl<B: AsyncRead + AsyncSeekStart> RangeBody for KnownSize<B> {
    fn byte_size(&self) -> u64 {
        self.byte_size
    }

    fn block_len(&self) -> u64 {
        self.block_len
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;
    use tokio::fs::File;
    use crate::RangeBody;

    use super::KnownSize;

    #[tokio::test]
    async fn test_file_size() {
        let file = PathBuf::from_str("test/fixture.txt").unwrap();
        let known_size = KnownSize::file(file).await.unwrap();
        assert_eq!(54, known_size.byte_size());
    }

    #[tokio::test]
    async fn test_seek_size() {
        let file = PathBuf::from_str("test/fixture.txt").unwrap();
        let known_size = KnownSize::file(file).await.unwrap();
        assert_eq!(54, known_size.byte_size());
    }
}
