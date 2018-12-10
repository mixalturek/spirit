//! A hello world service
//!
//! This version of a hello world service demonstrates a wide range of the possibilities and tools
//! spirit offers.
//!
//! It listens on one or more ports and greets with hello world (or other configured message) over
//! HTTP. It includes logging and daemonization.
//!
//! It allows reconfiguring everything at runtime ‒ change the config file(s), send SIGHUP to it
//! and it'll reload it.

extern crate hyper;
#[macro_use]
extern crate log;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate spirit;
extern crate spirit_daemonize;
extern crate spirit_hyper;
extern crate spirit_log;
extern crate spirit_tokio;
extern crate structopt;

use std::sync::Arc;

use hyper::{Body, Request, Response};
use spirit::Spirit;
use spirit_daemonize::{Daemon, Opts as DaemonOpts};
use spirit_hyper::HyperServer;
use spirit_log::{Cfg as Logging, Opts as LogOpts};
use spirit_tokio::{ExtraCfgCarrier, TcpListen};
use spirit_tokio::either::Either;
#[cfg(unix)]
use spirit_tokio::net::unix::UnixListen;
use structopt::StructOpt;

/// The command line arguments we would like our application to have.
///
/// Here we build it from prefabricated fragments provided by the `spirit-*` crates. Of course we
/// could also roll our own.
///
/// The spirit will add some more options on top of that ‒ it'll be able to accept
/// `--config-override` to override one or more config option on the command line and it'll accept
/// an optional list of config files and config directories.
#[derive(Clone, Debug, StructOpt)]
struct Opts {
    // Adds the `--daemonize` and `--foreground` options.
    #[structopt(flatten)]
    daemon: DaemonOpts,

    // Adds the `--log` and `--log-module` options.
    #[structopt(flatten)]
    log: LogOpts,
}

impl Opts {
    fn daemon(&self) -> &DaemonOpts {
        &self.daemon
    }
    fn logging(&self) -> LogOpts {
        self.log.clone()
    }
}

/// An application specific configuration.
///
/// For the Hello World Service, we configure just the message to send.
#[derive(Clone, Debug, Default, Deserialize)]
struct Ui {
    msg: String,
}

/// Similarly, each transport we listen on will carry its own signature.
///
/// Well, optional signature. It may be missing.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct Signature {
    signature: Option<String>,
}

/// Configuration of a http server.
///
/// The `HttpServer` could be enough. It would allow configuring the listening port and a whole
/// bunch of other details about listening (how many accepting tasks there should be in parallel,
/// on what interface to listen, TCP keepalive, HTTP keepalive...).
///
/// But we actually want something even more crazy. We want our users to be able to use on both
/// normal http over TCP on some port as well as on unix domain sockets. Don't say you've never
/// heard of HTTP over unix domain sockets...
///
/// So when the user puts `port = 1234`, it listens on TCP. If there's `path =
/// "/tmp/path/to/socket"`, it listens on http.
///
/// We also bundle the optional signature inside of that thing.
#[cfg(unix)]
type ListenSocket = Either<TcpListen<Signature>, UnixListen<Signature>>;
#[cfg(not(unix))]
type ListenSocket = TcpListen<Signature>;
type Server = HyperServer<ListenSocket>;

/// Putting the whole configuration together.
#[derive(Clone, Debug, Default, Deserialize)]
struct Cfg {
    /// Deamonization stuff
    ///
    /// Like the user to switch to, working directory or if it should actually daemonize.
    #[serde(default)]
    daemon: Daemon,

    /// The logging.
    ///
    /// This allows multiple logging destinations in parallel, configuring the format, timestamp
    /// format, destination.
    #[serde(flatten)]
    log: Logging,

    /// Yes, we allow to listen on multiple ports/interfaces at once.
    listen: Vec<Server>,

    /// And the message to send.
    ui: Ui,
}

impl Cfg {
    fn daemon(&self) -> Daemon {
        self.daemon.clone()
    }
    fn logging(&self) -> Logging {
        self.log.clone()
    }
    fn listen(&self) -> Vec<Server> {
        self.listen.clone()
    }
}

/// Let's bake some configuration in.
///
/// We wouldn't have to do that, but bundling a piece of configuration inside makes sure we can
/// start without one.
const DEFAULT_CONFIG: &str = r#"
[daemon]
pid-file = "/tmp/hws"
workdir = "/"

[[logging]]
level = "DEBUG"
type = "stderr"
clock = "UTC"
per-module = { hws_complete = "TRACE", hyper = "INFO", tokio = "INFO" }
format = "extended"

[[listen]]
port = 5678
host = "127.0.0.1"
http-mode = "http1-only"
backlog = 256
scale = 2
signature = "IPv4"

[[listen]]
port = 5678
host = "::1"
http-mode = "http1-only"
backlog = 256
scale = 2
only-v6 = true
signature = "IPv6"

[[listen]]
# This one will be rejected on Windows, because it'll turn off the unix domain socket support.
path = "/tmp/hws.socket"
http-mode = "http1-only"
backlog = 256
scale = 2

[ui]
msg = "Hello world"
"#;

/// This is the actual workhorse of the application.
///
/// This thing handles one request. The plumbing behind the scenes give it access to the relevant
/// parts of config.
fn hello(
    spirit: &Arc<Spirit<Opts, Cfg>>,
    cfg: &Arc<Server>,
    req: Request<Body>,
) -> Result<Response<Body>, std::io::Error> {
    trace!("Handling request {:?}", req);
    // Get some global configuration
    let mut msg = format!("{}\n", spirit.config().ui.msg);
    // Get some listener-local configuration.
    if let Some(ref signature) = cfg.extra().signature {
        msg.push_str(&format!("Brought to you by {}\n", signature));
    }
    Ok(Response::new(Body::from(msg)))
}

/// Putting it all together and starting.
fn main() {
    Spirit::<Opts, Cfg>::new()
        // The baked in configuration.
        .config_defaults(DEFAULT_CONFIG)
        // In addition to specifying configuration in files and command line, also allow overriding
        // it through an environment variable. This is useful to passing secrets in many
        // deployments (like all these docker based clouds).
        .config_env("HELLO")
        // If passed a directory, look for files with these extensions and load them as
        // configurations.
        //
        // Note that if a file is added or removed at runtime and the application receives SIGHUP,
        // the change is reflected.
        .config_exts(&["toml", "ini", "json"])
        // Plug in the daemonization configuration and command line arguments. The library will
        // make it alive ‒ it'll do the actual daemonization based on the config, it only needs to
        // be told it should do so this way.
        .config_helper(
            Cfg::daemon,
            spirit_daemonize::with_opts(Opts::daemon),
            "daemon",
        )
        // Similarly with logging.
        .config_helper(Cfg::logging, Opts::logging, "logging")
        // And with the HTTP servers. We pass the handler of one request, so it knows what to do
        // with it.
        .config_helper(Cfg::listen, spirit_hyper::server_configured(hello), "listen")
        // A custom callback ‒ when a new config is loaded, we want to print it to logs.
        .on_config(|cmd_line, new_cfg| {
            debug!("Current cmdline: {:?} and config {:?}", cmd_line, new_cfg);
        })
        // And run the application.
        //
        // Empty body here is fine. The rest of the work will happen afterwards, inside the HTTP
        // server.
        .run(|_spirit| Ok(()));
}