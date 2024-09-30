use std::{env, fs, path::PathBuf};

use prost::Message;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos: &[&str] = &["proto/validator/v1/validator.proto"];
    let includes: &[&str] = &["proto/validator/v1"];
    let file_descriptors = protox::compile(protos, includes)?;
    let file_descriptor_path =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR environment variable not set"))
            .join("file_descriptor_set.bin");
    fs::write(&file_descriptor_path, file_descriptors.encode_to_vec())?;

    let mut config = prost_build::Config::new();
    config.enable_type_names();
    tonic_build::configure()
        .skip_protoc_run()
        .file_descriptor_set_path(file_descriptor_path)
        .build_client(false)
        .compile_protos_with_config(config, protos, includes)?;
    Ok(())
}
