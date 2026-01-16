//! Order execution module.

pub mod leg;
pub mod types;

pub use leg::execute_leg;
pub use types::*;
