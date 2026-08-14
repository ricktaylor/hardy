/*!
Local-disk bundle storage backend for Hardy BPA.

Implements the [`hardy_bpa::storage::BundleStorage`] trait using the local filesystem.
Bundles are stored as individual files distributed across a two-level hexadecimal
directory structure (`xx/yy/`) to avoid filesystem bottlenecks from large flat directories.
An optional `fsync` mode provides crash-safe atomic writes via temp-file-and-rename.

# Key types

- [`LocalDiskStorage`] — the [`BundleStorage`](hardy_bpa::storage::BundleStorage) implementation.
*/

mod storage;

pub use self::storage::LocalDiskStorage;
