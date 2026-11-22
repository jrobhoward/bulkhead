#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A file secret whose reads return `reads` in turn (an error past them).
fn file(reads: &'static [&'static str]) -> Secret {
    let n = AtomicUsize::new(0);
    Secret::File {
        read: Box::new(move || {
            let i = n.fetch_add(1, Ordering::Relaxed);
            reads
                .get(i)
                .map(|r| r.as_bytes().to_vec())
                .ok_or_else(|| io::Error::other("gone"))
        }),
        last: Mutex::new(b"first".to_vec()),
    }
}

#[test]
fn current____file_rewritten____the_new_password_from_the_next_connection() {
    let secret = file(&["first", "second"]);

    let a = secret.current();
    let b = secret.current();

    assert_eq!(a.as_deref(), Some(&b"first"[..]));
    assert_eq!(b.as_deref(), Some(&b"second"[..]));
}

#[test]
fn current____file_unreadable____the_last_password_kept() {
    let secret = file(&["second"]);

    let a = secret.current();
    let b = secret.current();

    assert_eq!(a.as_deref(), Some(&b"second"[..]));
    assert_eq!(b.as_deref(), Some(&b"second"[..]));
}

#[test]
fn current____none_or_fixed____as_given() {
    assert_eq!(Secret::None.current(), None);
    assert_eq!(
        Secret::Fixed(b"x".to_vec()).current().as_deref(),
        Some(&b"x"[..])
    );
}
