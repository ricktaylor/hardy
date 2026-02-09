//! Synchronization primitives with platform-appropriate implementations.
//!
//! This module provides synchronization primitives organized by their characteristics:
//!
//! # Submodules
//!
//! - [`spin`] - Spinlock-based primitives for O(1) operations on hot paths
//!
//! # Future Additions
//!
//! - `Mutex` / `RwLock` - General-purpose locks (std::sync for std, embassy-sync for Embassy)
//! - `OnceLock` - One-time initialization primitive
//!
//! # Choosing the Right Primitive
//!
//! | Use Case | Primitive |
//! |----------|-----------|
//! | O(1) ops, hot path, no blocking | [`spin::Mutex`] |
//! | O(1) ops, read-heavy, no blocking | [`spin::RwLock`] |
//! | O(n) iteration, may block | `std::sync::Mutex` (future: `sync::Mutex`) |
//! | O(n) iteration, read-heavy | `std::sync::RwLock` (future: `sync::RwLock`) |

pub mod spin;
