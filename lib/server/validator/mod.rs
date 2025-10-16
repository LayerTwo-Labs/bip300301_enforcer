use tokio_util::sync::CancellationToken;

use crate::validator::Validator;

mod grpc;
pub mod json_rpc;

#[derive(Clone)]
pub struct Server {
    validator: Validator,
    cancel: CancellationToken,
}

impl Server {
    pub fn new(validator: Validator, cancel: CancellationToken) -> Self {
        Self { validator, cancel }
    }
}
