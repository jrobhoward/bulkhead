#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_native::protocol as p;

use super::{BlobError, BlobId};

#[test]
fn BlobId____as_text____round_trips() {
    let id = BlobId(42);

    let back: BlobId = id.to_string().parse().unwrap();

    assert_eq!(back, id);
}

#[test]
fn BlobError____from_the_protocol____each_kept() {
    let pairs = [
        (p::BlobError::Denied, BlobError::AccessDenied),
        (p::BlobError::NotFound, BlobError::NotFound),
        (p::BlobError::OverQuota, BlobError::OverQuota),
        (p::BlobError::TooLarge, BlobError::TooLarge),
        (p::BlobError::Mapped, BlobError::Mapped),
        (p::BlobError::NotSealed, BlobError::NotSealed),
        (
            p::BlobError::Other("x".into()),
            BlobError::Other("x".into()),
        ),
    ];

    for (from, to) in pairs {
        assert_eq!(BlobError::from(from), to);
    }
}
