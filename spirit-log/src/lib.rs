#![doc(
    html_root_url = "https://docs.rs/spirit-log/0.1.6/spirit_log/",
    test(attr(deny(warnings)))
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! A spirit configuration helper for logging.
//!
//! Set of configuration fragments and config helpers for the
//! [`spirit`](https://crates.io/crates/spirit) configuration framework that configures logging for
//! the [`log`](https://crates.io/crates/log) crate and updates it as the configuration changes
//! (even at runtime).
//!
//! Currently, it also allows asking for log output on the command line and multiple logging
//! destinations with different log levels set.
//!
//! It assumes the application doesn't set the global logger (this crate sets it on its own). It
//! also sets the panic hook through the [`log_panics`] crate. The `with-backtrace` cargo feature
//! is propagated through.
//!
//! For details about added options, see the [`Opts`](struct.Opts.html) and
//! [`Cfg`](struct.Cfg.html) configuration fragments.
//!
//! # Startup
//!
//! The logging is set in multiple steps:
//!
//! * As soon as the config helper is registered, a logging on the `WARN` level is sent to
//!   `stderr`.
//! * After command line arguments are parsed the `stderr` logging is updated to reflect that (or
//!   left on the `WARN` level if nothing is set by the user).
//! * After configuration is loaded from the files, full logging is configured.
//!
//! # Integration with other loggers
//!
//! If you need something specific (for example [`sentry`](https://crates.io/crates/sentry)), you
//! can provide functions to add additional loggers in parallel with the ones created in this
//! crate. The crate will integrate them all together.
//!
//! # Performance warning
//!
//! This allows the user to create arbitrary number of loggers. Furthermore, the logging is
//! synchronous and not buffered. When writing a lot of logs or sending them over the network, this
//! could become a bottleneck.
//!
//! # Planned features
//!
//! These pieces are planned some time in future, but haven't happened yet.
//!
//! * Reconnecting to the remote server if a TCP connection is lost.
//! * Log file rotation.
//! * Colors on `stdout`/`stderr`.
//! * Async and buffered logging and ability to drop log messages when logging doesn't keep up.
//!
//! # Examples
//!
//! ```rust
//! #[macro_use]
//! extern crate log;
//! extern crate spirit;
//! extern crate spirit_log;
//! #[macro_use]
//! extern crate serde_derive;
//! #[macro_use]
//! extern crate structopt;
//!
//! use spirit::Spirit;
//! use spirit_log::{Cfg as LogCfg, Opts as LogOpts};
//!
//! #[derive(Clone, Debug, StructOpt)]
//! struct Opts {
//!     #[structopt(flatten)]
//!     log: LogOpts,
//! }
//!
//! impl Opts {
//!     fn log(&self) -> LogOpts {
//!         self.log.clone()
//!     }
//! }
//!
//! #[derive(Clone, Debug, Default, Deserialize)]
//! struct Cfg {
//!     #[serde(flatten)]
//!     log: LogCfg,
//! }
//!
//! impl Cfg {
//!     fn log(&self) -> LogCfg {
//!         self.log.clone()
//!     }
//! }
//!
//! fn main() {
//!     Spirit::<Opts, Cfg>::new()
//!         .config_helper(Cfg::log, Opts::log, "logging")
//!         .run(|_spirit| {
//!             info!("Hello world");
//!             Ok(())
//!         });
//! }
//! ```
//!
//! The configuration could look something like this:
//!
//! ```toml
//! [[logging]]
//! level = "DEBUG"
//! type = "file"
//! filename = "/tmp/example.log"
//! clock = "UTC"
//! ```

extern crate chrono;
#[allow(unused_imports)]
#[macro_use]
extern crate failure;
extern crate fern;
extern crate itertools;
#[macro_use]
extern crate log;
extern crate log_panics;
extern crate log_reroute;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate spirit;
#[cfg(feature = "cfg-help")]
#[macro_use]
extern crate structdoc;
#[allow(unused_imports)]
#[macro_use]
extern crate structopt;
extern crate syslog;

use std::cmp;
use std::collections::HashMap;
use std::fmt::Arguments;
use std::io::{self, Write};
use std::iter;
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;

use chrono::format::{DelayedFormat, StrftimeItems};
use chrono::{Local, Utc};
use failure::{Error, Fail};
use fern::Dispatch;
use itertools::Itertools;
use log::{LevelFilter, Log, Metadata, Record};
use serde::de::{Deserialize, Deserializer, Error as DeError};
use serde::ser::{Serialize, Serializer};
use spirit::extension::{Extensible, Extension};
use spirit::fragment::driver::TrivialDriver;
use spirit::fragment::{Fragment, Installer};
use structopt::StructOpt;

pub struct MultiLog {
    max_level: LevelFilter,
    loggers: Vec<Box<Log>>,
}

impl MultiLog {
    pub fn push(&mut self, logger: Box<Log>, max_level: LevelFilter) {
        self.max_level = cmp::max(max_level, self.max_level);
        self.loggers.push(logger);
    }
    pub fn install(mut self) {
        debug!("Installing loggers");
        log::set_max_level(self.max_level);
        if self.loggers.len() == 1 {
            log_reroute::reroute_boxed(self.loggers.pop().unwrap());
        } else {
            log_reroute::reroute(self);
        }
    }
}

impl Default for MultiLog {
    fn default() -> Self {
        MultiLog {
            max_level: LevelFilter::Off,
            loggers: Vec::new(),
        }
    }
}

impl Log for MultiLog {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.loggers.iter().any(|l| l.enabled(metadata))
    }
    fn log(&self, record: &Record) {
        for sub in &self.loggers {
            sub.log(record)
        }
    }
    fn flush(&self) {
        for sub in &self.loggers {
            sub.flush()
        }
    }
}

/// A fragment for command line options.
///
/// By flattening this into the top-level `StructOpt` structure, you get the `-l` and `-L` command
/// line options. The `-l` (`--log`) sets the global logging level for `stderr`. The `-L` accepts
/// pairs (eg. `-L spirit=TRACE`) specifying levels for specific logging targets.
#[derive(Clone, Debug, StructOpt)]
pub struct Opts {
    /// Log to stderr with this log level.
    #[structopt(short = "l", long = "log", raw(number_of_values = "1"))]
    log: Option<LevelFilter>,

    /// Log to stderr with overriden levels for specific modules.
    #[structopt(
        short = "L",
        long = "log-module",
        parse(try_from_str = "spirit::key_val"),
        raw(number_of_values = "1")
    )]
    log_modules: Vec<(String, LevelFilter)>,
}

impl Opts {
    fn logger_cfg(&self) -> Option<Logger> {
        self.log.map(|level| Logger {
            level: LevelFilterSerde(level),
            destination: LogDestination::StdErr,
            per_module: self
                .log_modules
                .iter()
                .map(|(module, lf)| (module.clone(), LevelFilterSerde(*lf)))
                .collect(),
            clock: Clock::Local,
            time_format: cmdline_time_format(),
            format: Format::Short,
        })
    }
}

// TODO: OptsExt & OptsVerbose and turn the other things into Into<Opts>

#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "cfg-help", derive(StructDoc))]
#[serde(tag = "type", rename_all = "kebab-case")] // TODO: Make deny-unknown-fields work
enum LogDestination {
    /// Writes the logs into a file.
    File {
        /// The path to the file to store the log into.
        ///
        /// The file will be appended to or created if it doesn't exist. The directory it resides
        /// in must already exist.
        ///
        /// There is no direct support for log rotation. However, as the log file is reopened on
        /// `SIGHUP`, the usual external logrotate setup should work.
        filename: PathBuf,
        // TODO: Truncate
    },

    /// Sends the logs to local syslog.
    ///
    /// Note that syslog ignores formatting options.
    Syslog {
        /// Overrides the host value in the log messages.
        #[serde(skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        // TODO: Remote syslog
    },

    /// Sends the logs over a TCP connection over the network.
    Network {
        /// Hostname or IP address of the remote machine.
        host: String,

        /// Port to connect to on the remote machine.
        port: u16,
    },

    /// Writes logs to standard output.
    #[serde(rename = "stdout")]
    StdOut, // TODO: Colors

    /// Writes the logs to error output.
    #[serde(rename = "stderr")]
    StdErr, // TODO: Colors
}

const LEVEL_FILTERS: &[&str] = &["OFF", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

// A newtype to help us with serde, structdoc, default... more convenient inside maps and such.
#[derive(Copy, Clone, Debug)]
struct LevelFilterSerde(LevelFilter);

impl Default for LevelFilterSerde {
    fn default() -> LevelFilterSerde {
        LevelFilterSerde(LevelFilter::Error)
    }
}

impl<'de> Deserialize<'de> for LevelFilterSerde {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<LevelFilterSerde, D::Error> {
        let s = String::deserialize(d)?;
        s.parse()
            .map(LevelFilterSerde)
            .map_err(|_| D::Error::unknown_variant(&s, LEVEL_FILTERS))
    }
}

impl Serialize for LevelFilterSerde {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{:?}", self.0).to_uppercase())
    }
}

#[cfg(feature = "cfg-help")]
impl structdoc::StructDoc for LevelFilterSerde {
    fn document() -> structdoc::Documentation {
        use structdoc::{Documentation, Field, Tagging};

        let filters = LEVEL_FILTERS
            .iter()
            .map(|name| (*name, Field::new(Documentation::leaf_empty(), "")));
        Documentation::enum_(filters, Tagging::External)
    }
}

/// This error can be returned when initialization of logging to syslog fails.
#[derive(Debug, Fail)]
#[fail(display = "{}", _0)]
pub struct SyslogError(String);

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "cfg-help", derive(StructDoc))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Clock {
    Local,
    Utc,
}

impl Clock {
    fn now(self, format: &str) -> DelayedFormat<StrftimeItems> {
        match self {
            Clock::Local => Local::now().format(format),
            Clock::Utc => Utc::now().format(format),
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Clock::Local
    }
}

fn default_time_format() -> String {
    "%+".to_owned()
}

fn cmdline_time_format() -> String {
    "%F %T%.3f".to_owned()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "cfg-help", derive(StructDoc))]
#[serde(rename_all = "kebab-case")]
enum Format {
    /// Only the message, without any other fields.
    MessageOnly,
    /// The time, log level, log target and message in columns.
    Short,
    /// The time, log level, thread name, log target and message in columns.
    Extended,
    /// The time, log level, thread name, file name and line, log target and message in columns.
    Full,
    /// The time, log level, thread name, file name and line, log target and message in columns
    /// separated by tabs.
    ///
    /// This format is simpler to machine-parse (because the columns are separated by a single '\t'
    /// character and only the last one should ever contain it), but less human-readable because
    /// the columns don't have to visually align.
    Machine,
    /// The time, log level, thread name, file name and line, log target and message, formatted as
    /// json with these field names:
    ///
    /// * timestamp
    /// * level
    /// * thread_name
    /// * file
    /// * line
    /// * target
    /// * message
    ///
    /// Each message is on a separate line and the JSONs are not pretty-printed (therefore it is
    /// one JSON per line).
    // TODO: Configurable field names?
    Json,
    /// Similar to `json`, however with field names that correspond to default configuration of
    /// logstash.
    ///
    /// * @timestamp
    /// * @version (always set to 1)
    /// * level
    /// * thread_name
    /// * logger_name (corresponds to log target)
    /// * message
    Logstash,
    // TODO: Custom
}

impl Default for Format {
    fn default() -> Self {
        Format::Short
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "cfg-help", derive(StructDoc))]
#[serde(rename_all = "kebab-case")] // TODO: Make deny-unknown-fields work
struct Logger {
    #[serde(flatten)]
    destination: LogDestination,

    #[serde(default)]
    clock: Clock,

    /// The format of timestamp.
    ///
    /// This is strftime-like time format string, fully specified here:
    ///
    /// https://docs.rs/chrono/~0.4/chrono/format/strftime/index.html
    ///
    /// The default is %+, which corresponds to ISO 8601 / RFC 3339 date & time format.
    #[serde(default = "default_time_format")]
    time_format: String,

    /// Format of log messages.
    #[serde(default)]
    format: Format,

    /// The level on which to log messages.
    ///
    /// Messages with this level or more severe will be written into this logger.
    #[serde(default)]
    level: LevelFilterSerde,

    /// Overrides of log level per each module.
    ///
    /// The map allows for overriding log levels of each separate module (log target) separately.
    /// This allows silencing a verbose one or getting more info out of misbehaving one.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    per_module: HashMap<String, LevelFilterSerde>,
}

impl Logger {
    fn create(&self) -> Result<Dispatch, Error> {
        trace!("Creating logger for {:?}", self);
        let mut logger = Dispatch::new().level(self.level.0);
        logger = self
            .per_module
            .iter()
            .fold(logger, |logger, (module, level)| {
                logger.level_for(module.clone(), level.0)
            });
        let clock = self.clock;
        let time_format = self.time_format.clone();
        let format = self.format;
        match self.destination {
            // We don't want to format syslog
            LogDestination::Syslog { .. } => (),
            // We do with the other things
            _ => {
                logger = logger.format(move |out, message, record| {
                    match format {
                        Format::MessageOnly => out.finish(format_args!("{}", message)),
                        Format::Short => out.finish(format_args!(
                            "{} {:5} {:30} {}",
                            clock.now(&time_format),
                            record.level(),
                            record.target(),
                            message,
                        )),
                        Format::Extended => {
                            let thread = thread::current();
                            out.finish(format_args!(
                                "{} {:5} {:30} {:30} {}",
                                clock.now(&time_format),
                                record.level(),
                                thread.name().unwrap_or("<unknown>"),
                                record.target(),
                                message,
                            ));
                        }
                        Format::Full => {
                            let thread = thread::current();
                            out.finish(format_args!(
                                "{} {:5} {:10} {:>25}:{:<5} {:30} {}",
                                clock.now(&time_format),
                                record.level(),
                                thread.name().unwrap_or("<unknown>"),
                                record.file().unwrap_or("<unknown>"),
                                record.line().unwrap_or(0),
                                record.target(),
                                message,
                            ));
                        }
                        Format::Machine => {
                            let thread = thread::current();
                            out.finish(format_args!(
                                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                clock.now(&time_format),
                                record.level(),
                                thread.name().unwrap_or("<unknown>"),
                                record.file().unwrap_or("<unknown>"),
                                record.line().unwrap_or(0),
                                record.target(),
                                message,
                            ));
                        }
                        Format::Json => {
                            // We serialize it by putting things into a structure and using serde
                            // for that.
                            //
                            // This is a zero-copy structure.
                            #[derive(Serialize)]
                            struct Msg<'a> {
                                timestamp: Arguments<'a>,
                                level: Arguments<'a>,
                                thread_name: Option<&'a str>,
                                file: Option<&'a str>,
                                line: Option<u32>,
                                target: &'a str,
                                message: &'a Arguments<'a>,
                            }
                            // Unfortunately, the Arguments thing produced by format_args! doesn't
                            // like to live in a variable ‒ all attempts to put it into a let
                            // binding failed with various borrow-checker errors.
                            //
                            // However, constructing it as a temporary when calling a function
                            // seems to work fine. So we use this closure to work around the
                            // problem.
                            let log = |msg: &Msg| {
                                // TODO: Maybe use some shortstring or so here to avoid allocation?
                                let msg = serde_json::to_string(msg)
                                    .expect("Failed to serialize JSON log");
                                out.finish(format_args!("{}", msg));
                            };
                            let thread = thread::current();
                            log(&Msg {
                                timestamp: format_args!("{}", clock.now(&time_format)),
                                level: format_args!("{}", record.level()),
                                thread_name: thread.name(),
                                file: record.file(),
                                line: record.line(),
                                target: record.target(),
                                message,
                            });
                        }
                        Format::Logstash => {
                            // We serialize it by putting things into a structure and using serde
                            // for that.
                            //
                            // This is a zero-copy structure.
                            #[derive(Serialize)]
                            struct Msg<'a> {
                                #[serde(rename = "@timestamp")]
                                timestamp: Arguments<'a>,
                                #[serde(rename = "@version")]
                                version: u8,
                                level: Arguments<'a>,
                                thread_name: Option<&'a str>,
                                logger_name: &'a str,
                                message: &'a Arguments<'a>,
                            }
                            // Unfortunately, the Arguments thing produced by format_args! doesn't
                            // like to live in a variable ‒ all attempts to put it into a let
                            // binding failed with various borrow-checker errors.
                            //
                            // However, constructing it as a temporary when calling a function
                            // seems to work fine. So we use this closure to work around the
                            // problem.
                            let log = |msg: &Msg| {
                                // TODO: Maybe use some shortstring or so here to avoid allocation?
                                let msg = serde_json::to_string(msg)
                                    .expect("Failed to serialize JSON log");
                                out.finish(format_args!("{}", msg));
                            };
                            let thread = thread::current();
                            log(&Msg {
                                timestamp: format_args!("{}", clock.now(&time_format)),
                                version: 1,
                                level: format_args!("{}", record.level()),
                                thread_name: thread.name(),
                                logger_name: record.target(),
                                message,
                            });
                        }
                    }
                });
            }
        }
        match self.destination {
            LogDestination::File { ref filename } => Ok(logger.chain(fern::log_file(filename)?)),
            LogDestination::Syslog { ref host } => {
                let formatter = syslog::Formatter3164 {
                    facility: syslog::Facility::LOG_USER,
                    hostname: host.clone(),
                    // TODO: Does this give us the end-user crate or us?
                    process: env!("CARGO_PKG_NAME").to_owned(),
                    pid: 0,
                };
                // TODO: Other destinations than just unix
                Ok(logger
                    .chain(syslog::unix(formatter).map_err(|e| SyslogError(format!("{}", e)))?))
            }
            LogDestination::Network { ref host, port } => {
                // TODO: Reconnection support
                let conn = TcpStream::connect((&host as &str, port))?;
                Ok(logger.chain(Box::new(conn) as Box<Write + Send>))
            }
            LogDestination::StdOut => Ok(logger.chain(io::stdout())),
            LogDestination::StdErr => Ok(logger.chain(io::stderr())),
        }
    }
}

fn create<'a, I>(logging: I) -> Result<MultiLog, Error>
where
    I: IntoIterator<Item = &'a Logger>,
{
    debug!("Creating loggers");
    let (max_level, logger) = logging
        .into_iter()
        .map(Logger::create)
        .fold_results(Dispatch::new(), Dispatch::chain)?
        .into_log();
    Ok(MultiLog {
        max_level,
        loggers: vec![logger],
    })
}

/// A configuration fragment to set up logging.
///
/// By flattening this into the configuration structure, the program can load options for
/// configuring logging. It adds a new top-level array `logging`. Each item describes one logger,
/// with separate log levels and destination.
///
/// # Logger options
///
/// These are valid for all loggers:
///
/// * `level`: The log level to use. Valid options are `OFF`, `ERROR`, `WARN`, `INFO`, `DEBUG` and
///   `TRACE`.
/// * `per-module`: A map, setting log level overrides for specific modules (logging targets). This
///   one is optional.
/// * `type`: Specifies the type of logger destination. Some of them allow specifying other
///   options.
/// * `clock`: Either `LOCAL` or `UTC`. Defaults to `LOCAL` if not present.
/// * `time_format`: Time
///   [format string](https://docs.rs/chrono/*/chrono/format/strftime/index.html). Defaults to
///   `%+` (which is ISO 8601/RFC 3339). Note that the command line logger (one produced by `-l`)
///   uses a more human-friendly format.
/// * `format`: The format to use. There are few presets (and a custom may come in future).
///   - `message-only`: The line contains only the message itself.
///   - `short`: This is the default. `<timestamp> <level> <target> <message>`. Padded to form
///     columns.
///   - `extended`: <timestamp> <level> <thread-name> <target> <message>`. Padded to form columns.
///   - `full`: `<timestamp> <level> <thread-name> <file>:<line> <target> <message>`. Padded to
///     form columns.
///   - `machine`: Like `full`, but columns are not padded by spaces, they are separated by a
///     single `\t` character, for more convenient processing by tools like `cut`.
///   - `json`: The fields of `full` are encoded into a `json` format, for convenient processing of
///     more modern tools like logstash.
///   - `logstash`: `json` format with fields named and formatted according to
///     [Logback JSON encoder](https://github.com/logstash/logstash-logback-encoder#standard-fields)
///
/// The allowed types are:
/// * `stdout`: The logs are sent to standard output. There are no additional options.
/// * `stderr`: The logs are sent to standard error output. There are no additional options.
/// * `file`: Logs are written to a file. The file is reopened every time a configuration is
///   re-read (therefore every time the application gets `SIGHUP`), which makes it work with
///   logrotate.
///   - `filename`: The path to the file where to put the logs.
/// * `network`: The application connects to a given host and port over TCP and sends logs there.
///   - `host`: The hostname (or IP address) to connect to.
///   - `port`: The port to use.
/// * `syslog`: Sends the logs to syslog. This ignores all the formatting and time options, as
///   syslog handles this itself.
///
/// # Configuration helpers
///
/// This structure works as a configuration helper in three different forms:
///
/// ## No command line options.
///
/// There's no interaction with the command line options. The second parameter of the
/// `config_helper` is set to `()`.
///
/// ```rust
/// #[macro_use]
/// extern crate log;
/// extern crate spirit;
/// extern crate spirit_log;
/// #[macro_use]
/// extern crate serde_derive;
///
/// use spirit::{Empty, Spirit};
/// use spirit_log::Cfg as LogCfg;
///
/// #[derive(Clone, Debug, Default, Deserialize)]
/// struct Cfg {
///     #[serde(flatten)]
///     log: LogCfg,
/// }
///
/// impl Cfg {
///     fn log(&self) -> LogCfg {
///         self.log.clone()
///     }
/// }
///
/// fn main() {
///     Spirit::<Empty, Cfg>::new()
///         .config_helper(Cfg::log, (), "logging")
///         .run(|_spirit| {
///             info!("Hello world");
///             Ok(())
///         });
/// }
/// ```
///
/// ## Basic integration of command line options.
///
/// The second parameter is a closure to extract the [`Opts`](struct.Opts.html) structure from the
/// options (`Fn(&O) -> Opts + Send + Sync + 'static`).
///
/// ## Full customizations
///
/// The second parameter can be the [`Extras`](struct.Extras.html) structure, fully customizing the
/// creation of loggers.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "cfg-help", derive(StructDoc))]
pub struct Cfg {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    logging: Vec<Logger>,
}

struct Configured;

impl Cfg {
    pub fn init_extension<E: Extensible>() -> impl Extension<E> {
        |mut e: E| {
            if e.singleton::<Configured>() {
                log_panics::init();
                let logger = Logger {
                    destination: LogDestination::StdErr,
                    level: LevelFilterSerde(LevelFilter::Warn),
                    per_module: HashMap::new(),
                    clock: Clock::Local,
                    time_format: cmdline_time_format(),
                    format: Format::Short,
                };
                let _ = log_reroute::init();
                create(iter::once(&logger)).unwrap().install();
            }
            e
        }
    }
}

impl Fragment for Cfg {
    type Driver = TrivialDriver;
    type Seed = ();
    type Resource = MultiLog;
    type Installer = MultiLogInstaller;
    fn make_seed(&self, _name: &str) -> Result<(), Error> {
        Ok(())
    }
    fn make_resource(&self, _: &mut (), _name: &str) -> Result<MultiLog, Error> {
        create(&self.logging)
    }
    fn init<B: Extensible<Ok = B>>(builder: B, _name: &str) -> Result<B, Error> {
        builder.with(Cfg::init_extension())
    }
}

// TODO: Non-owned version too?
#[derive(Clone, Debug)]
pub struct CfgAndOpts {
    pub cfg: Cfg,
    pub opts: Opts,
}

impl Fragment for CfgAndOpts {
    type Driver = TrivialDriver;
    type Seed = ();
    type Resource = MultiLog;
    type Installer = MultiLogInstaller;
    const RUN_BEFORE_CONFIG: bool = true;
    fn make_seed(&self, _name: &str) -> Result<(), Error> {
        Ok(())
    }
    fn make_resource(&self, _: &mut (), _name: &str) -> Result<MultiLog, Error> {
        create(self.cfg.logging.iter().chain(self.opts.logger_cfg().as_ref()))
    }
    fn init<B: Extensible<Ok = B>>(builder: B, _name: &str) -> Result<B, Error> {
        builder.with(Cfg::init_extension())
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MultiLogInstaller;

impl<O, C> Installer<MultiLog, O, C> for MultiLogInstaller {
    type UninstallHandle = ();
    fn install(&mut self, logger: MultiLog) {
        logger.install();
    }
}
