//! Logging setup, functions, and utilities

use std::{
    io::Write as _,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use bip300301_enforcer_lib::cli::{Config, LogFormatter};
use miette::IntoDiagnostic as _;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    Registry, filter as tracing_filter, fmt::writer::BoxMakeWriter, layer::SubscriberExt as _,
    reload,
};

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

#[derive(Clone, Default)]
struct PendingLines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for PendingLines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type FileLayer =
    tracing_subscriber::fmt::Layer<Registry, LogFormatter, LogFormatter, BoxMakeWriter>;

/// The log directory is per chain, which is only known once the node has been
/// asked. Until [`Self::open`], file log lines are held in memory; if the
/// process ends first, they go to the data dir's log directory. Dropping this
/// flushes the log file, so keep it until the program exits.
pub struct FileLog {
    cli: Config,
    pending: PendingLines,
    handle: reload::Handle<FileLayer, Registry>,
    guard: Option<WorkerGuard>,
}

impl FileLog {
    /// Write to the log directory for `chain_data_dir`, starting with the
    /// held lines. Only the first call takes effect.
    pub fn open(&mut self, chain_data_dir: Option<&Path>) -> miette::Result<()> {
        if self.guard.is_some() {
            return Ok(());
        }
        let appender = self
            .cli
            .rolling_log_appender(&self.cli.log_dir(chain_data_dir))?;
        let (mut writer, guard) = tracing_appender::non_blocking(appender);
        let mut written = Ok(());
        // No line reaches the layer while it is being modified, so none is
        // lost or reordered. Logging in here would deadlock.
        self.handle
            .modify(|layer| {
                let pending = std::mem::take(
                    &mut *self
                        .pending
                        .0
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner),
                );
                written = writer.write_all(&pending);
                *layer.writer_mut() = BoxMakeWriter::new(writer);
            })
            .into_diagnostic()?;
        self.guard = Some(guard);
        written.into_diagnostic()
    }
}

impl Drop for FileLog {
    fn drop(&mut self) {
        if let Err(err) = self.open(None) {
            tracing::error!("failed to write log file: {err:#}");
        }
    }
}

// Configure logger.
pub fn set_tracing_subscriber(cli: &Config) -> miette::Result<FileLog> {
    let log_formatter = cli.log_formatter();
    let log_level = cli.logger_opts.level;
    let () = tracing_log::LogTracer::init().into_diagnostic()?;
    let targets_filter = {
        let default_directives_str = targets_directive_str([
            ("", saturating_pred_level(log_level)),
            ("bip300301", log_level),
            ("bip300301_enforcer", log_level),
            ("cusf_enforcer_mempool", log_level),
            (
                "h2",
                saturating_pred_level(saturating_pred_level(log_level)),
            ),
            (
                "hyper_util",
                saturating_pred_level(saturating_pred_level(log_level)),
            ),
            ("jsonrpsee_core::tracing", log_level),
            (
                "rustls",
                saturating_pred_level(saturating_pred_level(log_level)),
            ),
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

    let pending = PendingLines::default();
    let (file_layer, handle) = reload::Layer::new(
        tracing_subscriber::fmt::layer()
            .with_writer(BoxMakeWriter::new({
                let pending = pending.clone();
                move || pending.clone()
            }))
            .event_format(log_formatter.with_file(true).with_line_number(true))
            .fmt_fields(log_formatter)
            .with_ansi(false),
    );
    // The reload layer has to sit directly on the registry.
    let tracing_subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(targets_filter)
        .with(stdout_layer);

    tracing::subscriber::set_global_default(tracing_subscriber)
        .into_diagnostic()
        .map_err(|err| miette::miette!("setting default subscriber failed: {err:#}"))?;

    Ok(FileLog {
        cli: cli.clone(),
        pending,
        handle,
        guard: None,
    })
}
