//! Unit tests for small shared helpers.

use super::*;

#[test]
fn sizes() {
    assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
}
