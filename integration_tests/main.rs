use bip300301_enforcer_integration_tests::{integration_test, util::BinPaths};
use clap::Parser;
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    test_args: libtest_mimic::Arguments,
}

/// Saturating predecessor of a log level
fn saturating_pred_level(log_level: tracing::Level) -> tracing::Level {
    match log_level {
        tracing::Level::TRACE => tracing::Level::DEBUG,
        tracing::Level::DEBUG => tracing::Level::INFO,
        tracing::Level::INFO => tracing::Level::WARN,
        tracing::Level::WARN => tracing::Level::ERROR,
        tracing::Level::ERROR => tracing::Level::ERROR,
    }
}

/// The empty string target `""` can be used to set a default level.
fn targets_directive_str<'a, Targets>(targets: Targets) -> String
where
    Targets: IntoIterator<Item = (&'a str, tracing::Level)>,
{
    targets
        .into_iter()
        .map(|(target, level)| {
            let level = level.as_str().to_ascii_lowercase();
            if target.is_empty() {
                level
            } else {
                format!("{target}={level}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

// Configure logger.
fn set_tracing_subscriber(log_level: tracing::Level) -> anyhow::Result<()> {
    let targets_filter = {
        let default_directives_str = targets_directive_str([
            ("", saturating_pred_level(log_level)),
            ("integration_tests", log_level),
        ]);
        let directives_str = match std::env::var(tracing_filter::EnvFilter::DEFAULT_ENV) {
            Ok(env_directives) => format!("{default_directives_str},{env_directives}"),
            Err(std::env::VarError::NotPresent) => default_directives_str,
            Err(err) => return Err(err.into()),
        };
        tracing_filter::EnvFilter::builder().parse(directives_str)?
    };
    let indicatif_layer = tracing_indicatif::IndicatifLayer::new();
    let stdout_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_file(false)
        .with_line_number(false)
        .with_writer(indicatif_layer.get_stderr_writer());
    let tracing_subscriber = tracing_subscriber::registry()
        .with(targets_filter)
        .with(stdout_layer)
        .with(indicatif_layer);
    tracing::subscriber::set_global_default(tracing_subscriber)
        .map_err(|err| anyhow::anyhow!("setting default subscriber failed: {err:#}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    // Parse command line arguments
    let args = Cli::parse();
    let () = set_tracing_subscriber(tracing::Level::DEBUG)?;
    let rt_handle = tokio::runtime::Handle::current();
    // Read env vars
    if let Some(env_filepath) = std::env::var_os("BIP300301_ENFORCER_INTEGRATION_TEST_ENV") {
        let env_filepath: &std::path::Path = env_filepath.as_ref();
        tracing::info!("Adding env vars from `{}`", env_filepath.display());
        dotenvy::from_filename_override(env_filepath)?;
    }

    // Create a list of tests
    let mut tests = Vec::<libtest_mimic::Trial>::new();
    tests.extend(
        integration_test::tests(&BinPaths::from_env()?)
            .into_iter()
            .map(|trial| trial.run_blocking(rt_handle.clone())),
    );
    // Run all tests and exit the application appropriatly.
    let exit_code = libtest_mimic::run(&args.test_args, tests).exit_code();
    Ok(exit_code)
}
