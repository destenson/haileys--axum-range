# axum-range

HTTP range responses for [`axum`][1].

[Documentation][2].

Implementation of the [HTTP Range Requests (RFC9110)][3] ([RFC7233][4]) specification for the [`axum`][1] web framework.

MIT license.

### Example usage

```rust
use axum_extra::TypedHeader;
use axum_extra::headers::Range;

use tokio::fs::File;

use axum_range::Ranged;
use axum_range::KnownSize;

async fn file(range: Option<TypedHeader<Range>>) -> Ranged<KnownSize<File>> {
    let file = File::open("archlinux-x86_64.iso").await.unwrap();
    let body = KnownSize::file(file).await.unwrap();
    let range = range.map(|TypedHeader(range)| range);
    Ranged::new(range, body)
}
```

[1]: https://docs.rs/axum
[2]: https://docs.rs/axum-range
[3]: https://datatracker.ietf.org/doc/html/rfc9110#name-range-requests
[4]: https://datatracker.ietf.org/doc/html/rfc7233
