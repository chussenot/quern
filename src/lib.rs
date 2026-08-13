//! quern — a toy SQL engine. See docs/quern.md for the frozen contracts.
#![forbid(unsafe_code)]

pub mod catalog;
pub mod exec;
pub mod plan;
pub mod repl;
pub mod sql;
pub mod storage;
pub mod txn;
pub mod types;
