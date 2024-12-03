use std::{future::Future, path::PathBuf};

use clap::{Args, Parser};
use futures::TryFutureExt as _;
use libtest_mimic::{Failed, Trial};

#[derive(Args, Debug)]
struct BinPaths {
    #[arg(long)]
    bitcoind: PathBuf,
    #[arg(long)]
    bitcoin_cli: PathBuf,
    #[arg(long)]
    bip300301_enforcer: PathBuf,
    #[arg(long)]
    electrs: PathBuf,
}

mod integration_test;

#[derive(Parser)]
struct Cli {
    /// Path to the enforcer binary
    #[command(flatten)]
    bin_paths: BinPaths,
    #[command(flatten)]
    test_args: libtest_mimic::Arguments,
}

fn run_tokio_test<Err, F, Fut>(test: F) -> Result<(), Failed>
where
    Failed: From<Err>,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), Err>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { test().map_err(Failed::from).await })
}

fn main() {
    // Parse command line arguments
    let args = Cli::parse();
    // Create a list of tests
    let tests = vec![Trial::test("integration_test", move || {
        run_tokio_test(|| integration_test::test(&args.bin_paths))
    })];

    // Run all tests and exit the application appropriatly.
    libtest_mimic::run(&args.test_args, tests).exit();
}
