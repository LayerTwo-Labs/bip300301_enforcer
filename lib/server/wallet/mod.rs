// Re-export this with names that work better externally.
pub use crate::server::validator::Server as Validator;

mod grpc;
pub mod json_rpc;
