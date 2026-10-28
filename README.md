# axum-range

HTTP range responses for [`axum`][1].

[Documentation][2].

Implementation of the [HTTP Range Requests (RFC9110)][3] ([RFC7233][4]) specification for the [`axum`][1] web framework.

MIT license.

### Example usage

```rust
use std::path::PathBuf;

use tokio::fs::File;

use axum_range::{KnownSize, RangeRequest, Ranged};

async fn file(request: RangeRequest) -> Ranged<KnownSize<File>> {
    let file = PathBuf::from("archlinux-x86_64.iso");
    let body = KnownSize::file(file).await.unwrap();
    Ranged::from_request(request, body)
}
```

[1]: https://docs.rs/axum
[2]: https://docs.rs/axum-range
[3]: https://datatracker.ietf.org/doc/html/rfc9110#name-range-requests
[4]: https://datatracker.ietf.org/doc/html/rfc7233
