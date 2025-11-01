//! Storage layer for spatial phrase indexing.
//!
//! Provides efficient storage and querying of geographic phrase data using RocksDB.
//!
//! # Overview
//!
//! The storage layer consists of two main components:
//!
//! - **[`GridStoreBuilder`]**: Write-time interface for building indexes
//! - **[`GridStore`]**: Read-time interface for querying indexes
//!
//! # Performance Features
//!
//! - **Prefix bins**: Aggregate multiple phrases for efficient range queries
//! - **Memory-mapped reads**: Fast read access via mmap
//! - **Compressed encoding**: Relevance quantized to 4 bits, feature IDs to 24 bits
//! - **Grouped storage**: Entries grouped by relevance/score for query efficiency

mod builder;
mod coalesce;
mod common;
mod error;
mod spatial;
mod stackable;
mod store;

pub use builder::GridStoreBuilder;
pub use coalesce::{coalesce_single, CoalesceContext};
pub use common::*;
pub use error::*;
pub use spatial::*;
pub use stackable::{stackable, StackableNode, StackableTree, ArenaManager};
pub use store::GridStore;
