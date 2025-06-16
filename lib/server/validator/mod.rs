use crate::validator::Validator;

mod grpc;
pub mod json_rpc;

#[derive(Clone)]
pub struct Server {
    validator: Validator,
    shutdown_tx: futures::channel::mpsc::Sender<()>,
}

impl Server {
    pub fn new(validator: Validator, shutdown_tx: futures::channel::mpsc::Sender<()>) -> Self {
        Self {
            validator,
            shutdown_tx,
        }
    }
}
