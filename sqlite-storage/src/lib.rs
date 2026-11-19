/*!
SQLite-backed metadata storage for the Hardy BPA.

This crate provides a persistent [`MetadataStorage`](hardy_bpa::storage::MetadataStorage)
implementation that stores bundle metadata in a SQLite database. It handles
schema migrations, a single writer thread that batches concurrent writes
into shared transactions, pooled readers kept off the async runtime, and
the startup recovery protocol (mark-unconfirmed / confirm / sweep).

# Key types

- [`SqliteStorage`] -- the [`MetadataStorage`](hardy_bpa::storage::MetadataStorage) implementation.
*/

mod migrate;
mod readers;
mod storage;
mod writer;

pub use self::storage::SqliteStorage;
