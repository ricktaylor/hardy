/*!
SQLite-backed metadata storage for the Hardy BPA.

This crate provides a persistent [`MetadataStorage`](hardy_bpa::storage::MetadataStorage)
implementation that stores bundle metadata in a SQLite database. It handles
schema migrations, connection pooling, serialized write access, and the
startup recovery protocol (mark-unconfirmed / confirm / sweep).

# Key types

- [`SqliteStorage`] -- the [`MetadataStorage`](hardy_bpa::storage::MetadataStorage) implementation.
*/

mod migrate;
mod pool;
mod storage;

pub use self::storage::SqliteStorage;
