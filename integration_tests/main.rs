use bip300301_enforcer_integration_tests::{
    integration_test,
    util::{BinPaths, TestFailureCollector, TestFileRegistry, display_timing_summary},
};
use clap::Parser;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};

#[derive(Parser)]
struct Cli {
    /// Stream test-harness logs at this level while tests run: off, error,
    /// warn, info, debug, or trace. Off by default; the progress lines, the
    /// pass/fail summary, and per-failure log dumps are always shown.
    #[arg(long, default_value = "off", value_name = "LEVEL")]
    log_level: LevelFilter,
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

// Configure logger. `log_level` gates streaming of harness log events: `off`
// (the default) streams nothing — the progress lines, results, and per-failure
// dumps are printed directly, not via tracing — and any other level streams the
// harness crate at that level (deps one level lower).
fn set_tracing_subscriber(log_level: LevelFilter) -> anyhow::Result<()> {
    let Some(level) = log_level.into_level() else {
        return Ok(());
    };
    let targets_filter = {
        let default_directives_str = targets_directive_str([
            ("", saturating_pred_level(level)),
            ("bip300301_enforcer_integration_tests", level),
        ]);
        let directives_str = match std::env::var(tracing_filter::EnvFilter::DEFAULT_ENV) {
            Ok(env_directives) => format!("{default_directives_str},{env_directives}"),
            Err(std::env::VarError::NotPresent) => default_directives_str,
            Err(err) => return Err(err.into()),
        };
        tracing_filter::EnvFilter::builder().parse(directives_str)?
    };
    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_file(false)
        .with_line_number(false)
        .with_writer(std::io::stderr);
    let tracing_subscriber = tracing_subscriber::registry()
        .with(targets_filter)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(tracing_subscriber)
        .map_err(|err| anyhow::anyhow!("setting default subscriber failed: {err:#}"))
}

/// Whether `trial` would run under `args`, mirroring libtest_mimic's
/// name-based filtering (substring match, or exact with `--exact`, minus any
/// `--skip` patterns). Lets us detect an empty filter result before running.
fn filter_matches(args: &libtest_mimic::Arguments, trial: &libtest_mimic::Trial) -> bool {
    let name = trial.name();
    if let Some(filter) = &args.filter {
        let hit = if args.exact {
            name == filter
        } else {
            name.contains(filter)
        };
        if !hit {
            return false;
        }
    }
    for skip in &args.skip {
        let hit = if args.exact {
            name == skip
        } else {
            name.contains(skip)
        };
        if hit {
            return false;
        }
    }
    true
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            #[expect(clippy::print_stderr)]
            {
                eprintln!("Error: {err:#}");
            }
            std::process::ExitCode::from(1)
        }
    }
}

async fn run() -> anyhow::Result<std::process::ExitCode> {
    // Parse command line arguments
    let args = Cli::parse();
    let () = set_tracing_subscriber(args.log_level)?;
    let rt_handle = tokio::runtime::Handle::current();
    // Read env vars
    if let Some(env_filepath) = std::env::var_os("BIP300301_ENFORCER_INTEGRATION_TEST_ENV") {
        let env_filepath: &std::path::Path = env_filepath.as_ref();
        tracing::debug!("Adding env vars from `{}`", env_filepath.display());
        dotenvy::from_filename_override(env_filepath).map_err(|err| {
            anyhow::anyhow!(
                "Failed to load env vars from `{}`: {err:#}",
                env_filepath.display()
            )
        })?;

        tracing::info!("Loaded env vars from `{}`", env_filepath.display());
    }

    let file_registry = TestFileRegistry::new();
    let failure_collector = TestFailureCollector::new();

    // Binary paths are resolved lazily on first access for tests or
    // subcommands that need them, pushing out any errors until
    // they're actually necessary
    let bin_paths = BinPaths::new();

    // Create a list of tests
    let mut tests = Vec::<libtest_mimic::Trial>::new();
    tests.extend(
        integration_test::tests(&bin_paths, file_registry, failure_collector.clone())
            .into_iter()
            .map(|trial| trial.run_blocking(rt_handle.clone())),
    );

    // Bail *before* running if a filter was provided but matches nothing —
    // otherwise libtest prints its "running 0 tests" banner and an "ok" result
    // line before we get a chance to error out.
    if args.test_args.filter.is_some()
        && !tests
            .iter()
            .any(|trial| filter_matches(&args.test_args, trial))
    {
        anyhow::bail!(
            "no integration test matched the provided filter `{}`",
            args.test_args.filter.as_deref().unwrap_or("")
        );
    }

    // Run all tests and collect the exit code
    let started = std::time::Instant::now();
    let conclusion = libtest_mimic::run(&args.test_args, tests);
    let wall = started.elapsed();
    let exit_code = conclusion.exit_code();

    // Per-test timing, then any failures at the end
    display_timing_summary(wall);
    failure_collector.display_all_failures();

    Ok(exit_code)
}
