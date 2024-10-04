pub mod validator {
    tonic::include_proto!("validator.v1");
}

pub mod mainchain {
    tonic::include_proto!("mainchain.v1");
}

pub mod sidechain {
    tonic::include_proto!("sidechain");
}
