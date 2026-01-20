//! Polymarket CLOB client and types.

pub mod client;
pub mod types;

// Re-export client types
pub use client::{
    OrderArgs, OrderStruct, PolymarketAsyncClient, SharedAsyncClient, SignedOrder,
    POLYMARKET_CLOB_HOST,
};
pub use types::*;
