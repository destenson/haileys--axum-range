
use axum::serve;
use axum::{Router, routing::get};
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum_extra::TypedHeader;
use serde::{Deserialize, Serialize};
use axum_range::{Ranged, KnownSize, RangeBody, RangedStream};
use axum_extra::headers::{ContentLength, Range};

#[tokio::main]
async fn main() {

    let router = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/file", get(get_file));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, router).await.unwrap();
    // // Open a file asynchronously
    // let file = File::open("test/fixture.txt").await.unwrap();
    // 
    // // Read the file size
    // let mut metadata = file.metadata().await.unwrap();
    // let file_size = metadata.len();
    // 
    // // Seek to the end of the file
    // let mut file = file;
    // file.seek(io::SeekFrom::End(0)).await.unwrap();
    // 
    // println!("File size: {} bytes", file_size);
}

fn handle_error(err: io::Error) -> String {
    format!("An error occurred: {}", err)
}

// fn serve_file(file: File) -> impl axum::response::IntoResponse {
//     async move {
//         let mut buffer = Vec::new();
//         if let Err(e) = file.read_to_end(&mut buffer).await {
//             return Err(handle_error(e));
//         }
//         Ok(axum::response::Response::builder()
//             .header("Content-Length", buffer.len())
//             .body(buffer.into())
//             .unwrap())
//     }
// }

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
struct FileRequest {
    path: String,
}

async fn get_file(
    range_header: Option<TypedHeader<Range>>,
    Query(q): Query<FileRequest>
) -> impl IntoResponse {
    match PathBuf::from_str(&q.path) {
        Ok(file) => {
            if !file.exists() {
                (StatusCode::NOT_FOUND, "File not found").into_response()
            } else {
                match KnownSize::file(file).await {
                    Ok(body) => {
                        let range = range_header.map(|TypedHeader(range)| range);
                        Ranged::new(range, body).into_response()
                    }
                    Err(e) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {}", e)).into_response()
                    }
                }
            }
        }
        Err(e) => {
            (StatusCode::NOT_FOUND, format!("File not found: {}", e)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use axum_range::extract_boundary;

    const FIXTURE: &str = "test/fixture_abc.txt";
    const CONTENT: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    #[tokio::test]
    async fn test_get_file_with_range() {
        let app = Router::new().route("/file", get(get_file));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/file?path={FIXTURE}");

        // single range
        let response = client.get(&url).header("Range", "bytes=0-4").send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["Content-Range"], "bytes 0-4/62");
        assert_eq!(response.text().await.unwrap(), "01234");

        // multiple ranges: multipart/byteranges, parts in request order, and a
        // Content-Length covering the whole multipart body
        let response = client.get(&url).header("Range", "bytes=-1,0-4").send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        let content_type = response.headers()["Content-Type"].to_str().unwrap().to_string();
        assert!(content_type.starts_with("multipart/byteranges;"), "{content_type}");
        let boundary = extract_boundary(&content_type).unwrap();
        let content_length: usize = response.headers()["Content-Length"].to_str().unwrap().parse().unwrap();
        let body = response.bytes().await.unwrap();
        assert_eq!(content_length, body.len());

        let stream = futures::stream::once(async move { Ok::<_, Infallible>(body) });
        let mut multipart = multer::Multipart::new(stream, boundary);
        let mut parts = Vec::new();
        while let Some(field) = multipart.next_field().await.unwrap() {
            let content_range = field.headers()["Content-Range"].to_str().unwrap().to_string();
            let content_type = field.headers()["Content-Type"].to_str().unwrap().to_string();
            parts.push((content_range, content_type, field.text().await.unwrap()));
        }
        assert_eq!(parts, [
            ("bytes 61-61/62".to_string(), "text/plain".to_string(), "z".to_string()),
            ("bytes 0-4/62".to_string(), "text/plain".to_string(), "01234".to_string()),
        ]);

        // unsatisfiable range
        let response = client.get(&url).header("Range", "bytes=1000000-2000000").send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()["Content-Range"], "bytes */62");

        // no range: the whole file
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), CONTENT);

        server.abort();
    }
}

// pub async fn parse_multipart_response(
//     body: Bytes,
//     boundary: &str
// ) -> Result<Vec<(String, Bytes)>, multer::Error> {
//     
//     use multer::bytes::BytesMut;
//     
//     let body = BytesMut::from(body);
//     let mut multipart = multer::Multipart::new(body, boundary);
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
// }
