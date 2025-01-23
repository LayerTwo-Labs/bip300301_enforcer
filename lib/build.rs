use std::{
    env, fs,
    path::{Path, PathBuf},
};

use prost::Message;

fn compile_protos_with_config<F>(
    file_descriptor_path: impl AsRef<Path>,
    protos: &[impl AsRef<Path>],
    includes: &[impl AsRef<Path>],
    config_fn: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut prost_build::Config) -> Result<(), Box<dyn std::error::Error>>,
{
    let mut config = prost_build::Config::new();
    config.enable_type_names();
    let () = config_fn(&mut config)?;
    tonic_build::configure()
        .skip_protoc_run()
        .file_descriptor_set_path(file_descriptor_path)
        .compile_protos_with_config(config, protos, includes)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const COMMON_PROTO: &str = "../cusf_sidechain_proto/proto/cusf/common/v1/common.proto";
    const CRYPTO_PROTO: &str = "../cusf_sidechain_proto/proto/cusf/crypto/v1/crypto.proto";
    const MAINCHAIN_COMMON_PROTO: &str =
        "../cusf_sidechain_proto/proto/cusf/mainchain/v1/common.proto";
    const SIDECHAIN_PROTO: &str = "../cusf_sidechain_proto/proto/cusf/sidechain/v1/sidechain.proto";
    const VALIDATOR_PROTO: &str = "../cusf_sidechain_proto/proto/cusf/mainchain/v1/validator.proto";
    const WALLET_PROTO: &str = "../cusf_sidechain_proto/proto/cusf/mainchain/v1/wallet.proto";
    const ALL_PROTOS: &[&str] = &[
        COMMON_PROTO,
        CRYPTO_PROTO,
        MAINCHAIN_COMMON_PROTO,
        SIDECHAIN_PROTO,
        VALIDATOR_PROTO,
        WALLET_PROTO,
    ];
    const INCLUDES: &[&str] = &["../cusf_sidechain_proto/proto"];
    let file_descriptors = protox::compile(ALL_PROTOS, INCLUDES)?;
    let file_descriptor_path =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR environment variable not set"))
            .join("file_descriptor_set.bin");
    fs::write(&file_descriptor_path, file_descriptors.encode_to_vec())?;

    let () =
        compile_protos_with_config(&file_descriptor_path, &[COMMON_PROTO], INCLUDES, |_| Ok(()))?;
    let () = compile_protos_with_config(
        &file_descriptor_path,
        &[
            CRYPTO_PROTO,
            MAINCHAIN_COMMON_PROTO,
            SIDECHAIN_PROTO,
            VALIDATOR_PROTO,
            WALLET_PROTO,
        ],
        INCLUDES,
        |config| {
            config.extern_path(".cusf.common.v1", "crate::proto::common");
            Ok(())
        },
    )?;
    Ok(())
}
