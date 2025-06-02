
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
                        let content_type = body.content_type();
                        // If we have a range header, use it
                        if let Some(TypedHeader(range)) = range_header {
                            Ranged::new(Some(range), body, content_type).into_response()
                        } else {
                            Ranged::new(None, body, content_type).into_response()
                        }
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
    use axum::Router;
    use axum::routing::get;
    use std::net::SocketAddr;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;
    use axum_range::extract_boundary;

    #[tokio::test]
    async fn test_get_file_with_range() {
        std::env::set_var("RUST_LOG", "debug");
        tracing_subscriber::fmt::init();
        // Create a temporary file for testing
        let test_file_path = "../video-browser/output-1.mp4";
        // let test_file_path = "test_file.txt";
        // let mut file = File::create(test_file_path).await.unwrap();
        // file.write_all(b"Hello, World!").await.unwrap();
        // drop(file); // Close the file before the server tries to open it

        // Set up the router
        let app = Router::new().route("/file", get(get_file));

        // Start the server
        let addr = SocketAddr::from(([127, 0, 0, 1], 3001));
        let server_handle = tokio::spawn(async move {
            axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app.into_make_service()).await.unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;


        // Create a client
        let client = reqwest::Client::new();

        // Test with a valid range
        let response = client
            .get(format!("http://{}/file?path={}", addr, test_file_path))
            .header("Range", "bytes=0-4")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        let body = response.bytes().await.unwrap();
        assert_eq!(body.len(), 5);

        // Test with a split range
        let response = client
            .get(format!("http://{}/file?path={}", addr, test_file_path))
            .header("Range", "bytes=0-4,-1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        println!("Response headers: {:?}", response.headers());
        
        let content_type = response.headers().get("Content-Type").unwrap().to_str().unwrap();
        let boundary = extract_boundary(content_type).unwrap();
        assert!(!boundary.is_empty(), "Boundary should be present in the Content-Type header");
        // // assert_eq!(response.headers().get("Content-Range").unwrap(), "bytes 0-4,-1");
        // let parts = parse_multipart_response(body, &boundary).await.unwrap();
        // assert_eq!(parts.len(), 2);
        // assert_eq!(parts[0].1.len(), 5); // bytes 0-4
        // assert_eq!(parts[1].1.len(), 5); // bytes 10-14


        // Test with a split range
        println!("Testing split range");
        let response = client
            .get(format!("http://{}/file?path={}", addr, test_file_path))
            .header("Range", "bytes=0-4,-1/*")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        println!("Response headers: {:?}", response.headers());

        let content_type = response.headers().get("Content-Type").unwrap().to_str().unwrap();
        let boundary = extract_boundary(content_type).unwrap();
        assert!(!boundary.is_empty(), "Boundary should be present in the Content-Type header");
        // // assert_eq!(response.headers().get("Content-Range").unwrap(), "bytes 0-4,-1");
        // let parts = parse_multipart_response(body, &boundary).await.unwrap();
        // assert_eq!(parts.len(), 2);
        // assert_eq!(parts[0].1.len(), 5); // bytes 0-4
        // assert_eq!(parts[1].1.len(), 5); // bytes 10-14

        let body = response.bytes().await.unwrap();
        assert_eq!(body.len(), 6);

        
        // Test with an invalid range
        let response = client
            .get(format!("http://{}/file?path={}", addr, test_file_path))
            .header("Range", "bytes=1000000-2000000")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::RANGE_NOT_SATISFIABLE);

        // Test without a range header (full file)
        let response = client
            .get(format!("http://{}/file?path={}", addr, test_file_path))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.bytes().await.unwrap();
        assert_eq!(body, "Hello, World!");

        tokio::time::sleep(tokio::time::Duration::from_secs(20)).await; // Wait for the server to finish processing

        // Clean up
        tokio::fs::remove_file(test_file_path).await.unwrap();
        server_handle.abort(); // Stop the server
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
