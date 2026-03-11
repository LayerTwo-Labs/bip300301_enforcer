//! Logging setup, functions, and utilities

use bip300301_enforcer_lib::cli::LogFormatter;
use miette::IntoDiagnostic as _;
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt as _};

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

// Configure logger. The returned guard should be dropped when the program
// exits.
pub fn set_tracing_subscriber(
    log_formatter: LogFormatter,
    log_level: tracing::Level,
    rolling_log_appender: tracing_appender::rolling::RollingFileAppender,
) -> miette::Result<tracing_appender::non_blocking::WorkerGuard> {
    let () = tracing_log::LogTracer::init().into_diagnostic()?;
    let targets_filter = {
        let default_directives_str = targets_directive_str([
            ("", saturating_pred_level(log_level)),
            ("bip300301", log_level),
            ("cusf_enforcer_mempool", log_level),
            ("jsonrpsee_core::tracing", log_level),
            ("bip300301_enforcer", log_level),
        ]);
        let directives_str = match std::env::var(tracing_filter::EnvFilter::DEFAULT_ENV) {
            Ok(env_directives) => format!("{default_directives_str},{env_directives}"),
            Err(std::env::VarError::NotPresent) => default_directives_str,
            Err(err) => return Err(err).into_diagnostic(),
        };
        tracing_filter::EnvFilter::builder()
            .parse(directives_str)
            .into_diagnostic()?
    };
    // If no writer is provided (as here!), logs end up at stdout.
    let mut stdout_layer = tracing_subscriber::fmt::layer()
        .event_format(log_formatter.with_file(true).with_line_number(true))
        .fmt_fields(log_formatter);
    let is_terminal = std::io::IsTerminal::is_terminal(&stdout_layer.writer()());
    stdout_layer.set_ansi(is_terminal);

    // Ensure the appender is non-blocking!
    let (file_appender, guard) = tracing_appender::non_blocking(rolling_log_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .event_format(log_formatter.with_file(true).with_line_number(true))
        .fmt_fields(log_formatter)
        .with_ansi(false);
    let tracing_subscriber = tracing_subscriber::registry()
        .with(targets_filter)
        .with(stdout_layer)
        .with(file_layer);

    tracing::subscriber::set_global_default(tracing_subscriber)
        .into_diagnostic()
        .map_err(|err| miette::miette!("setting default subscriber failed: {err:#}"))?;

    Ok(guard)
}
