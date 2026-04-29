use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use prost::Message;

fn get_git_hash() -> Result<String, Box<dyn std::error::Error>> {
    let args = ["describe", "--always", "--dirty"];
    let output = Command::new("git").args(args).output()?;

    if !output.status.success() {
        return Err(format!("Failed to execute `git {}`", args.join(" ")).into());
    }

    let hash = String::from_utf8(output.stdout)?.trim().to_string();

    Ok(hash)
}

fn compile_protos_with_config<F, P>(
    file_descriptor_path: impl AsRef<Path>,
    protos: &[P],
    includes: &[P],
    config_fn: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut prost_build::Config) -> Result<(), Box<dyn std::error::Error>>,
    P: AsRef<Path>,
{
    let mut config = prost_build::Config::new();
    config.enable_type_names();
    let () = config_fn(&mut config)?;
    tonic_prost_build::configure()
        .skip_protoc_run()
        .file_descriptor_set_path(file_descriptor_path)
        .compile_with_config(config, protos, includes)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set git hash as environment variable for runtime access
    match get_git_hash() {
        Ok(hash) => {
            println!("cargo:rustc-env=GIT_HASH={hash}");
            println!("cargo:rerun-if-changed=.git/HEAD");
            println!("cargo:rerun-if-changed=.git/refs/heads");
        }
        Err(e) => {
            println!("cargo:warning=Failed to get git hash: {e:#}");
            println!("cargo:rustc-env=GIT_HASH=unknown");
        }
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let workspace_root = manifest_dir
        .parent()
        .expect("lib crate must live under bip300301_enforcer workspace root");
    let proto_include = [
        workspace_root.join("cusf_sidechain_proto/proto"),
        workspace_root.join("../cusf_sidechain_proto/proto"),
    ]
    .into_iter()
    .find(|p| p.join("cusf/common/v1/common.proto").is_file())
    .expect(
        "cusf_sidechain_proto/proto not found; run `git submodule update --init --recursive` \
         or place cusf_sidechain_proto next to the workspace root",
    );
    let proto = |rel: &str| proto_include.join(rel);
    let common_proto = proto("cusf/common/v1/common.proto");
    let crypto_proto = proto("cusf/crypto/v1/crypto.proto");
    let mainchain_common_proto = proto("cusf/mainchain/v1/common.proto");
    let sidechain_proto = proto("cusf/sidechain/v1/sidechain.proto");
    let validator_proto = proto("cusf/mainchain/v1/validator.proto");
    let wallet_proto = proto("cusf/mainchain/v1/wallet.proto");
    let all_protos: Vec<PathBuf> = vec![
        common_proto.clone(),
        crypto_proto.clone(),
        mainchain_common_proto.clone(),
        sidechain_proto.clone(),
        validator_proto.clone(),
        wallet_proto.clone(),
    ];
    let includes: [&Path; 1] = [proto_include.as_path()];
    let file_descriptors = protox::compile(
        &all_protos.iter().map(|p| p.as_path()).collect::<Vec<_>>(),
        &includes,
    )?;
    let file_descriptor_path =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR environment variable not set"))
            .join("file_descriptor_set.bin");
    fs::write(&file_descriptor_path, file_descriptors.encode_to_vec())?;

    let () = compile_protos_with_config(
        &file_descriptor_path,
        &[common_proto.as_path()],
        &includes,
        |_| Ok(()),
    )?;
    let () = compile_protos_with_config(
        &file_descriptor_path,
        &[
            crypto_proto.as_path(),
            mainchain_common_proto.as_path(),
            sidechain_proto.as_path(),
            validator_proto.as_path(),
            wallet_proto.as_path(),
        ],
        &includes,
        |config| {
            config.extern_path(".cusf.common.v1", "crate::proto::common");
            Ok(())
        },
    )?;
    Ok(())
}
