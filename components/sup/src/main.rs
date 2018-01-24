// Copyright (c) 2017-2017 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate habitat_common as common;
#[macro_use]
extern crate habitat_core as hcore;
extern crate habitat_launcher_client as launcher_client;
#[macro_use]
extern crate habitat_sup as sup;
#[macro_use]
extern crate log;
extern crate env_logger;
extern crate ansi_term;
#[macro_use]
extern crate lazy_static;
extern crate libc;
#[macro_use]
extern crate clap;
extern crate futures;
extern crate protobuf;
extern crate pbr;
extern crate time;
extern crate url;
extern crate tabwriter;
extern crate tokio_core;

use std::env;
use std::io::{self, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process;
use std::result;
use std::str::FromStr;

use clap::{App, ArgMatches};
use common::command::package::install::InstallSource;
use common::ui::{NONINTERACTIVE_ENVVAR, UI, Coloring};
use futures::prelude::*;
use hcore::channel;
use hcore::crypto::{self, default_cache_key_path, SymKey};
#[cfg(windows)]
use hcore::crypto::dpapi::encrypt;
use hcore::env as henv;
use hcore::package::PackageIdent;
use hcore::service::{ApplicationEnvironment, ServiceGroup};
use hcore::url::{bldr_url_from_env, default_bldr_url};
use launcher_client::{LauncherCli, ERR_NO_RETRY_EXCODE, OK_NO_RETRY_EXCODE};
use tabwriter::TabWriter;
use tokio_core::reactor;
use url::Url;

use sup::VERSION;
use sup::config::{GossipListenAddr, GOSSIP_DEFAULT_PORT};
use sup::ctl_gateway;
use sup::ctl_gateway::client::{SrvClient, SrvClientError};
use sup::ctl_gateway::codec::*;
use sup::error::{Error, Result, SupError};
use sup::feat;
use sup::command;
use sup::http_gateway;
use sup::http_gateway::ListenAddr;
use sup::manager::{Manager, ManagerConfig};
use sup::manager::service::{ServiceBind, Topology, UpdateStrategy};
use sup::protocols;
use sup::util;

/// Our output key
static LOGKEY: &'static str = "MN";

static RING_ENVVAR: &'static str = "HAB_RING";
static RING_KEY_ENVVAR: &'static str = "HAB_RING_KEY";

lazy_static! {
    static ref STATUS_HEADER: Vec<&'static str> = {
        vec!["package", "type", "state", "uptime (s)", "pid", "group"]
    };
}

fn main() {
    if let Err(err) = start() {
        println!("{}", err);
        match err {
            SupError { err: Error::ProcessLocked(_), .. } => process::exit(ERR_NO_RETRY_EXCODE),
            SupError { err: Error::Departed, .. } => {
                process::exit(ERR_NO_RETRY_EXCODE);
            }
            _ => process::exit(1),
        }
    }
}

fn boot() -> Option<LauncherCli> {
    env_logger::init();
    enable_features_from_env();
    if !crypto::init() {
        println!("Crypto initialization failed!");
        process::exit(1);
    }
    match launcher_client::env_pipe() {
        Some(pipe) => {
            match LauncherCli::connect(pipe) {
                Ok(launcher) => Some(launcher),
                Err(err) => {
                    println!("{}", err);
                    process::exit(1);
                }
            }
        }
        None => None,
    }
}

fn start() -> Result<()> {
    let launcher = boot();
    let app_matches = match cli().get_matches_safe() {
        Ok(matches) => matches,
        Err(err) => {
            let out = io::stdout();
            writeln!(&mut out.lock(), "{}", err.message).expect("Error writing Error to stdout");
            process::exit(ERR_NO_RETRY_EXCODE);
        }
    };
    match app_matches.subcommand() {
        ("bash", Some(m)) => sub_bash(m),
        ("config", Some(m)) => sub_config(m),
        ("load", Some(m)) => sub_load(m),
        ("run", Some(m)) => {
            let launcher = launcher.ok_or(sup_error!(Error::NoLauncher))?;
            sub_run(m, launcher)
        }
        ("sh", Some(m)) => sub_sh(m),
        ("start", Some(m)) => sub_start(m),
        ("status", Some(m)) => sub_status(m),
        ("stop", Some(m)) => sub_stop(m),
        ("term", Some(m)) => sub_term(m),
        ("unload", Some(m)) => sub_unload(m),
        _ => unreachable!(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cli<'a, 'b>() -> App<'a, 'b> {
    clap_app!(("hab-sup") =>
        (about: "The Habitat Supervisor")
        (version: VERSION)
        (author: "\nAuthors: The Habitat Maintainers <humans@habitat.sh>\n")
        (@setting VersionlessSubcommands)
        (@setting SubcommandRequiredElseHelp)
        (@arg VERBOSE: -v +global "Verbose output; shows line numbers")
        (@arg NO_COLOR: --("no-color") +global "Turn ANSI color off")
        (@subcommand bash =>
            (about: "Start an interactive Bash-like shell")
            (aliases: &["b", "ba", "bas"])
        )
        (@subcommand config =>
            (about: "Displays the default configuration options for a service")
            (aliases: &["c", "co", "con", "conf", "confi"])
            (@arg PKG_IDENT: +required +takes_value "A package identifier (ex: core/redis)")
        )
        (@subcommand load =>
            (about: "Load a service to be started and supervised by Habitat from a package \
                identifier. If an installed package doesn't satisfy the given package identifier, \
                a suitable package will be installed from Builder.")
            (aliases: &["lo", "loa"])
            (@arg PKG_IDENT: +required +takes_value
                "A Habitat package identifier (ex: core/redis)")
            (@arg APPLICATION: --application -a +takes_value requires[ENVIRONMENT]
                "Application name; [default: not set].")
            (@arg ENVIRONMENT: --environment -e +takes_value requires[APPLICATION]
                "Environment name; [default: not set].")
            (@arg CHANNEL: --channel +takes_value
                "Receive package updates from the specified release channel [default: stable]")
            (@arg GROUP: --group +takes_value
                "The service group; shared config and topology [default: default].")
            (@arg BLDR_URL: --url -u +takes_value {valid_url}
                "Receive package updates from Builder at the specified URL \
                [default: https://bldr.habitat.sh]")
            (@arg TOPOLOGY: --topology -t +takes_value {valid_topology}
                "Service topology; [default: none]")
            (@arg STRATEGY: --strategy -s +takes_value {valid_update_strategy}
                "The update strategy; [default: none] [values: none, at-once, rolling]")
            (@arg BIND: --bind +takes_value +multiple
                "One or more service groups to bind to a configuration")
            (@arg FORCE: --force -f "Load or reload an already loaded service. If the service was \
                previously loaded and running this operation will also restart the service")
        )
        (@subcommand unload =>
            (about: "Unload a service loaded by the Habitat Supervisor. If the service is running \
                it will additionally be stopped.")
            (aliases: &["un", "unl", "unlo", "unloa"])
            (@arg PKG_IDENT: +required +takes_value "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand run =>
            (about: "Run the Habitat Supervisor")
            (aliases: &["r", "ru"])
            (@arg LISTEN_GOSSIP: --("listen-gossip") +takes_value {valid_listen_gossip}
                "The listen address for the gossip system [default: 0.0.0.0:9638]")
            (@arg LISTEN_HTTP: --("listen-http") +takes_value {valid_listen_http}
                "The listen address for the HTTP Gateway [default: 0.0.0.0:9631]")
            (@arg LISTEN_CTL: --("listen-ctl") +takes_value {valid_listen_http}
                "The listen address for the Control Gateway [default: 0.0.0.0:9632]")
            (@arg NAME: --("override-name") +takes_value
                "The name of the Supervisor if launching more than one [default: default]")
            (@arg ORGANIZATION: --org +takes_value
                "The organization that the Supervisor and its subsequent services are part of \
                [default: default]")
            (@arg PEER: --peer +takes_value +multiple
                "The listen address of an initial peer (IP[:PORT])")
            (@arg PERMANENT_PEER: --("permanent-peer") -I "If this Supervisor is a permanent peer")
            (@arg PEER_WATCH_FILE: --("peer-watch-file") +takes_value conflicts_with[peer]
                "Watch this file for connecting to the ring"
            )
            (@arg RING: --ring -r +takes_value "Ring key name")
            (@arg CHANNEL: --channel +takes_value
                "Receive Supervisor updates from the specified release channel [default: stable]")
            (@arg BLDR_URL: --url -u +takes_value {valid_url}
                "Receive Supervisor updates from Builder at the specified URL \
                [default: https://bldr.habitat.sh]")
            (@arg AUTO_UPDATE: --("auto-update") -A "Enable automatic updates for the Supervisor \
                itself")
            (@arg EVENTS: --events -n +takes_value {valid_service_group} "Name of the service \
                group running a Habitat EventSrv to forward Supervisor and service event data to")
            // === Optional arguments to additionally load an initial service for the Supervisor 
            (@arg PKG_IDENT_OR_ARTIFACT: +takes_value "Load the given Habitat package as part of \
                the Supervisor startup specified by a package identifier \
                (ex: core/redis) or filepath to a Habitat Artifact \
                (ex: /home/core-redis-3.0.7-21120102031201-x86_64-linux.hart).")
            (@arg APPLICATION: --application -a +takes_value requires[ENVIRONMENT]
                "Application name; [default: not set].")
            (@arg ENVIRONMENT: --environment -e +takes_value requires[APPLICATION]
                "Environment name; [default: not set].")
            (@arg GROUP: --group +takes_value
                "The service group; shared config and topology [default: default].")
            (@arg TOPOLOGY: --topology -t +takes_value {valid_topology}
                "Service topology; [default: none]")
            (@arg STRATEGY: --strategy -s +takes_value {valid_update_strategy}
                "The update strategy; [default: none] [values: none, at-once, rolling]")
            (@arg BIND: --bind +takes_value +multiple
                "One or more service groups to bind to a configuration")
            (@arg FORCE: --force -f "Load or reload an already loaded service. If the service was \
                previously loaded and running this operation will also restart the service")
        )
        (@subcommand sh =>
            (about: "Start an interactive Bourne-like shell")
            (aliases: &[])
        )
        (@subcommand start =>
            (about: "Start a loaded, but stopped, Habitat service.")
            (aliases: &["sta", "star"])
            (@arg PKG_IDENT: +required +takes_value
                "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand status =>
            (about: "Query the status of Habitat services.")
            (aliases: &["stat", "statu", "status"])
            (@arg PKG_IDENT: +takes_value "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand stop =>
            (about: "Stop a running Habitat service.")
            (aliases: &["sto"])
            (@arg PKG_IDENT: +required +takes_value "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand term =>
            (about: "Gracefully terminate the Habitat Supervisor and all of its running services")
            (@arg NAME: --("override-name") +takes_value
                "The name of the Supervisor if more than one is running [default: default]")
        )
    )
}

#[cfg(target_os = "windows")]
fn cli<'a, 'b>() -> App<'a, 'b> {
    clap_app!(("hab-sup") =>
        (about: "The Habitat Supervisor")
        (version: VERSION)
        (author: "\nAuthors: The Habitat Maintainers <humans@habitat.sh>\n")
        (@setting VersionlessSubcommands)
        (@setting SubcommandRequiredElseHelp)
        (@arg VERBOSE: -v +global "Verbose output; shows line numbers")
        (@arg NO_COLOR: --("no-color") +global "Turn ANSI color off")
        (@subcommand bash =>
            (about: "Start an interactive Bash-like shell")
            (aliases: &["b", "ba", "bas"])
        )
        (@subcommand config =>
            (about: "Displays the default configuration options for a service")
            (aliases: &["c", "co", "con", "conf", "confi"])
            (@arg PKG_IDENT: +required +takes_value
                "A package identifier (ex: core/redis, core/busybox-static/1.42.2)")
        )
        (@subcommand load =>
            (about: "Load a service to be started and supervised by Habitat from a package \
                identifier. If an installed package doesn't satisfy the given package identifier, \
                a suitable package will be installed from Builder.")
            (aliases: &["lo", "loa"])
            (@arg PKG_IDENT: +required +takes_value
                "A Habitat package identifier (ex: core/redis)")
            (@arg APPLICATION: --application -a +takes_value requires[ENVIRONMENT]
                "Application name; [default: not set].")
            (@arg ENVIRONMENT: --environment -e +takes_value requires[APPLICATION]
                "Environment name; [default: not set].")
            (@arg CHANNEL: --channel +takes_value
                "Receive package updates from the specified release channel [default: stable]")
            (@arg GROUP: --group +takes_value
                "The service group; shared config and topology [default: default].")
            (@arg BLDR_URL: --url -u +takes_value {valid_url}
                "Receive package updates from Builder at the specified URL \
                [default: https://bldr.habitat.sh]")
            (@arg TOPOLOGY: --topology -t +takes_value {valid_topology}
                "Service topology; [default: none]")
            (@arg STRATEGY: --strategy -s +takes_value {valid_update_strategy}
                "The update strategy; [default: none] [values: none, at-once, rolling]")
            (@arg BIND: --bind +takes_value +multiple
                "One or more service groups to bind to a configuration")
            (@arg FORCE: --force -f "Load or reload an already loaded service. If the service was \
                previously loaded and running this operation will also restart the service")
            (@arg PASSWORD: --password +takes_value "Password of the service user")
        )
        (@subcommand unload =>
            (about: "Unload a service loaded by the Habitat Supervisor. If the service is running \
                it will additionally be stopped.")
            (aliases: &["un", "unl", "unlo", "unloa"])
            (@arg PKG_IDENT: +required +takes_value "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand run =>
            (about: "Run the Habitat Supervisor")
            (aliases: &["r", "ru"])
            (@arg LISTEN_GOSSIP: --("listen-gossip") +takes_value {valid_listen_gossip}
                "The listen address for the gossip system [default: 0.0.0.0:9638]")
            (@arg LISTEN_HTTP: --("listen-http") +takes_value {valid_listen_http}
                "The listen address for the HTTP Gateway [default: 0.0.0.0:9631]")
            (@arg LISTEN_CTL: --("listen-ctl") +takes_value {valid_listen_http}
                "The listen address for the Control Gateway [default: 0.0.0.0:9632]")
            (@arg NAME: --("override-name") +takes_value
                "The name of the Supervisor if launching more than one [default: default]")
            (@arg ORGANIZATION: --org +takes_value
                "The organization that the Supervisor and its subsequent services are part of \
                [default: default]")
            (@arg PEER: --peer +takes_value +multiple
                "The listen address of an initial peer (IP[:PORT])")
            (@arg PERMANENT_PEER: --("permanent-peer") -I "If this Supervisor is a permanent peer")
            (@arg PEER_WATCH_FILE: --("peer-watch-file") +takes_value conflicts_with[peer]
                "Watch this file for connecting to the ring"
            )
            (@arg RING: --ring -r +takes_value "Ring key name")
            (@arg CHANNEL: --channel +takes_value
                "Receive Supervisor updates from the specified release channel [default: stable]")
            (@arg BLDR_URL: --url -u +takes_value {valid_url}
                "Receive Supervisor updates from Builder at the specified URL \
                [default: https://bldr.habitat.sh]")
            (@arg AUTO_UPDATE: --("auto-update") -A "Enable automatic updates for the Supervisor \
                itself")
            (@arg EVENTS: --events -n +takes_value {valid_service_group} "Name of the service \
                group running a Habitat EventSrv to forward Supervisor and service event data to")
            // === Optional arguments to additionally load an initial service for the Supervisor 
            (@arg PKG_IDENT_OR_ARTIFACT: +takes_value "Load the given Habitat package as part of \
                the Supervisor startup specified by a package identifier \
                (ex: core/redis) or filepath to a Habitat Artifact \
                (ex: /home/core-redis-3.0.7-21120102031201-x86_64-linux.hart).")
            (@arg APPLICATION: --application -a +takes_value requires[ENVIRONMENT]
                "Application name; [default: not set].")
            (@arg ENVIRONMENT: --environment -e +takes_value requires[APPLICATION]
                "Environment name; [default: not set].")
            (@arg GROUP: --group +takes_value
                "The service group; shared config and topology [default: default].")
            (@arg TOPOLOGY: --topology -t +takes_value {valid_topology}
                "Service topology; [default: none]")
            (@arg STRATEGY: --strategy -s +takes_value {valid_update_strategy}
                "The update strategy; [default: none] [values: none, at-once, rolling]")
            (@arg BIND: --bind +takes_value +multiple
                "One or more service groups to bind to a configuration")
            (@arg FORCE: --force -f "Load or reload an already loaded service. If the service was \
                previously loaded and running this operation will also restart the service")
            (@arg PASSWORD: --password +takes_value "Password of the service user")
        )
        (@subcommand sh =>
            (about: "Start an interactive Bourne-like shell")
            (aliases: &[])
        )
        (@subcommand start =>
            (about: "Start a loaded, but stopped, Habitat service.")
            (aliases: &["sta", "star"])
            (@arg PKG_IDENT: +required +takes_value
                "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand status =>
            (about: "Query the status of Habitat services.")
            (aliases: &["stat", "statu", "status"])
            (@arg PKG_IDENT: +takes_value "A Habitat package identifier (ex: core/redis)")
        )
        (@subcommand stop =>
            (about: "Stop a running Habitat service.")
            (aliases: &["sto"])
            (@arg PKG_IDENT: +required +takes_value "A Habitat package identifier (ex: core/redis)")
        )
    )
}

fn sub_bash(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);

    command::shell::bash()
}

fn sub_config(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);
    let ident = PackageIdent::from_str(m.value_of("PKG_IDENT").unwrap())?;
    let cfg = mgrcfg_from_matches(m)?;
    let mut core = reactor::Core::new().unwrap();
    let mut msg = protocols::ctl::SvcGetDefaultCfg::new();
    msg.set_ident(ident.into());
    let conn = SrvClient::connect(&cfg.ctl_listen, ctl_secret_key(&cfg)?)
        .wait()?;
    let req = conn.call(msg).for_each(|reply| {
        match reply.message_id() {
            "ServiceCfg" => {
                let m = reply.parse::<protocols::types::ServiceCfg>().unwrap();
                println!("{}", m.get_default());
            }
            "NetErr" => {
                let m = reply.parse::<protocols::net::NetErr>().unwrap();
                println!("{}", m);
            }
            _ => (),
        }
        Ok(())
    });
    core.run(req)?;
    Ok(())
}

fn sub_load(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);
    let cfg = mgrcfg_from_matches(m)?;
    let mut core = reactor::Core::new().unwrap();
    let mut msg = protocols::ctl::SvcLoad::new();
    update_svc_load_from_input(m, &mut msg)?;
    let ident: PackageIdent = m.value_of("PKG_IDENT").unwrap().parse()?;
    msg.set_ident(ident.into());
    let conn = SrvClient::connect(&cfg.ctl_listen, ctl_secret_key(&cfg)?)
        .wait()?;
    let req = conn.call(msg).for_each(handle_ctl_reply);
    core.run(req)?;
    Ok(())
}

fn sub_unload(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);
    let ident = PackageIdent::from_str(m.value_of("PKG_IDENT").unwrap())?;
    let cfg = mgrcfg_from_matches(m)?;
    let mut core = reactor::Core::new().unwrap();
    let mut msg = protocols::ctl::SvcUnload::new();
    msg.set_ident(ident.into());
    let conn = SrvClient::connect(&cfg.ctl_listen, ctl_secret_key(&cfg)?)
        .wait()?;
    let req = conn.call(msg).for_each(handle_ctl_reply);
    core.run(req)?;
    Ok(())
}

fn sub_run(m: &ArgMatches, launcher: LauncherCli) -> Result<()> {
    let cfg = mgrcfg_from_matches(m)?;
    if Manager::is_running(&cfg)? {
        process::exit(OK_NO_RETRY_EXCODE);
    } else {
        let manager = Manager::load(cfg, launcher)?;
        // We need to determine if we have an initial service to start
        let svc = if let Some(pkg) = m.value_of("PKG_IDENT_OR_ARTIFACT") {
            let mut msg = protocols::ctl::SvcLoad::new();
            update_svc_load_from_input(m, &mut msg)?;
            let ident = match pkg.parse::<InstallSource>()? {
                source @ InstallSource::Archive(_) => {
                    // Install the archive manually then explicitly set the pkg ident to the
                    // version found in the archive. This will lock the software to this
                    // specific version.
                    let install = util::pkg::install(
                        &mut ui(),
                        msg.get_bldr_url(),
                        &source,
                        msg.get_bldr_channel(),
                    )?;
                    install.ident.into()
                }
                InstallSource::Ident(ident) => ident.into(),
            };
            msg.set_ident(ident);
            Some(msg)
        } else {
            None
        };
        manager.run(svc)
    }
}

fn sub_sh(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);

    command::shell::sh()
}

fn sub_start(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);
    let ident = PackageIdent::from_str(m.value_of("PKG_IDENT").unwrap())?;
    let cfg = mgrcfg_from_matches(m)?;
    let mut core = reactor::Core::new().unwrap();
    let mut msg = protocols::ctl::SvcStart::new();
    msg.set_ident(ident.into());
    let conn = SrvClient::connect(&cfg.ctl_listen, ctl_secret_key(&cfg)?)
        .wait()?;
    let req = conn.call(msg).for_each(handle_ctl_reply);
    core.run(req)?;
    Ok(())
}

fn sub_status(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);
    let cfg = mgrcfg_from_matches(m)?;
    let mut core = reactor::Core::new().unwrap();
    let mut msg = protocols::ctl::SvcStatus::new();
    // Note that PKG_IDENT is NOT required here
    if let Some(pkg) = m.value_of("PKG_IDENT") {
        let ident = PackageIdent::from_str(pkg)?;
        msg.set_ident(ident.into());
    }
    let conn = SrvClient::connect(&cfg.ctl_listen, ctl_secret_key(&cfg)?)
        .wait()?;
    let mut out = TabWriter::new(io::stdout());
    let req = conn.call(msg)
        .into_future()
        .map_err(|(err, _)| err)
        .and_then(move |(reply, rest)| {
            match reply {
                None => {
                    return Err(SrvClientError::from(
                        io::Error::from(io::ErrorKind::UnexpectedEof),
                    ))
                }
                Some(m) => print_svc_status(&mut out, m, true)?,
            }
            Ok((out, rest))
        })
        .and_then(|(mut out, rest)| {
            rest.for_each(move |reply| print_svc_status(&mut out, reply, false))
        });
    core.run(req)?;
    Ok(())
}

fn print_svc_status<T>(
    out: &mut T,
    reply: SrvMessage,
    print_header: bool,
) -> result::Result<(), SrvClientError>
where
    T: io::Write,
{
    let mut status = match reply.message_id() {
        "ServiceStatus" => reply.parse::<protocols::types::ServiceStatus>()?,
        "NetOk" => {
            println!("No services loaded.");
            return Ok(());
        }
        "NetErr" => {
            let err = reply.parse::<protocols::net::NetErr>()?;
            return Err(SrvClientError::from(err));
        }
        _ => {
            warn!("Unexpected status message, {:?}", reply);
            return Ok(());
        }
    };
    let svc_type = if status.has_composite() {
        status.take_composite()
    } else {
        "standalone".to_string()
    };
    let svc_pid = if status.get_process().has_pid() {
        status.get_process().get_pid().to_string()
    } else {
        "<none>".to_string()
    };
    if print_header {
        write!(out, "{}\n", STATUS_HEADER.join("\t")).unwrap();
    }
    write!(
        out,
        "{}\t{}\t{}\t{}\t{}\t{}\n",
        status.get_ident(),
        svc_type,
        status.get_process().get_state(),
        status.get_process().get_elapsed(),
        svc_pid,
        status.get_service_group(),
    )?;
    out.flush()?;
    return Ok(());
}

fn sub_stop(m: &ArgMatches) -> Result<()> {
    toggle_verbosity(m);
    toggle_color(m);
    let ident = PackageIdent::from_str(m.value_of("PKG_IDENT").unwrap())?;
    let cfg = mgrcfg_from_matches(m)?;
    let mut core = reactor::Core::new().unwrap();
    let mut msg = protocols::ctl::SvcStop::new();
    msg.set_ident(ident.into());
    let conn = SrvClient::connect(&cfg.ctl_listen, ctl_secret_key(&cfg)?)
        .wait()?;
    let req = conn.call(msg).for_each(handle_ctl_reply);
    core.run(req)?;
    Ok(())
}

fn sub_term(m: &ArgMatches) -> Result<()> {
    let cfg = mgrcfg_from_matches(m)?;
    match Manager::term(&cfg) {
        Err(SupError { err: Error::ProcessLockIO(_, _), .. }) => {
            println!("Supervisor not started.");
            Ok(())
        }
        result => result,
    }
}

// Internal Implementation Details
////////////////////////////////////////////////////////////////////////

fn mgrcfg_from_matches(m: &ArgMatches) -> Result<ManagerConfig> {
    let mut cfg = ManagerConfig::default();
    cfg.auto_update = m.is_present("AUTO_UPDATE");
    cfg.update_url = bldr_url(m);
    cfg.update_channel = channel(m);
    if let Some(addr_str) = m.value_of("LISTEN_GOSSIP") {
        cfg.gossip_listen = GossipListenAddr::from_str(addr_str)?;
    }
    if let Some(addr_str) = m.value_of("LISTEN_HTTP") {
        cfg.http_listen = http_gateway::ListenAddr::from_str(addr_str)?;
    }
    if let Some(addr_str) = m.value_of("LISTEN_CTL") {
        cfg.ctl_listen =
            SocketAddr::from_str(addr_str).unwrap_or_else(|_err| ctl_gateway::default_addr());
    }
    if let Some(name_str) = m.value_of("NAME") {
        cfg.name = Some(String::from(name_str));
        outputln!("");
        outputln!("CAUTION: Running more than one Habitat Supervisor is not recommended for most");
        outputln!("CAUTION: users in most use cases. Using one Supervisor per host for multiple");
        outputln!("CAUTION: services in one ring will yield much better performance.");
        outputln!("");
        outputln!("CAUTION: If you know what you're doing, carry on!");
        outputln!("");
    }
    cfg.organization = m.value_of("ORGANIZATION").map(|org| org.to_string());
    cfg.gossip_permanent = m.is_present("PERMANENT_PEER");
    // TODO fn: Clean this up--using a for loop doesn't feel good however an iterator was
    // causing a lot of developer/compiler type confusion
    let mut gossip_peers: Vec<SocketAddr> = Vec::new();
    if let Some(peers) = m.values_of("PEER") {
        for peer in peers {
            let peer_addr = if peer.find(':').is_some() {
                peer.to_string()
            } else {
                format!("{}:{}", peer, GOSSIP_DEFAULT_PORT)
            };
            let addrs: Vec<SocketAddr> = match peer_addr.to_socket_addrs() {
                Ok(addrs) => addrs.collect(),
                Err(e) => {
                    outputln!("Failed to resolve peer: {}", peer_addr);
                    return Err(sup_error!(Error::NameLookup(e)));
                }
            };
            let addr: SocketAddr = addrs[0];
            gossip_peers.push(addr);
        }
    }
    cfg.gossip_peers = gossip_peers;
    if let Some(watch_peer_file) = m.value_of("PEER_WATCH_FILE") {
        cfg.watch_peer_file = Some(String::from(watch_peer_file));
    }
    let ring = match m.value_of("RING") {
        Some(val) => Some(SymKey::get_latest_pair_for(
            &val,
            &default_cache_key_path(None),
        )?),
        None => {
            match henv::var(RING_KEY_ENVVAR) {
                Ok(val) => {
                    let (key, _) =
                        SymKey::write_file_from_str(&val, &default_cache_key_path(None))?;
                    Some(key)
                }
                Err(_) => {
                    match henv::var(RING_ENVVAR) {
                        Ok(val) => {
                            Some(SymKey::get_latest_pair_for(
                                &val,
                                &default_cache_key_path(None),
                            )?)
                        }
                        Err(_) => None,
                    }
                }
            }
        }
    };
    if let Some(ring) = ring {
        cfg.ring = Some(ring.name_with_rev());
    }
    if let Some(events) = m.value_of("EVENTS") {
        cfg.eventsrv_group = ServiceGroup::from_str(events).ok();
    }
    Ok(cfg)
}

// Various CLI Parsing Functions
////////////////////////////////////////////////////////////////////////

/// Resolve a Builder URL. Taken from CLI args, the environment, or
/// (failing those) a default value.
fn bldr_url(m: &ArgMatches) -> String {
    match bldr_url_from_input(m) {
        Some(url) => url.to_string(),
        None => default_bldr_url(),
    }
}

/// A Builder URL, but *only* if the user specified it via CLI args or
/// the environment
fn bldr_url_from_input(m: &ArgMatches) -> Option<String> {
    m.value_of("BLDR_URL")
        .and_then(|u| Some(u.to_string()))
        .or_else(|| bldr_url_from_env())
}

/// Resolve a channel. Taken from CLI args, or (failing that), a
/// default value.
fn channel(matches: &ArgMatches) -> String {
    channel_from_input(matches).unwrap_or(channel::default())
}

/// A channel name, but *only* if the user specified via CLI args.
fn channel_from_input(m: &ArgMatches) -> Option<String> {
    m.value_of("CHANNEL").and_then(|c| Some(c.to_string()))
}

// ServiceSpec Modification Functions
////////////////////////////////////////////////////////////////////////

fn get_group_from_input(m: &ArgMatches) -> Option<String> {
    m.value_of("GROUP").map(ToString::to_string)
}

/// If the user provides both --application and --environment options,
/// parse and set the value on the spec.
fn get_app_env_from_input(m: &ArgMatches) -> Result<Option<ApplicationEnvironment>> {
    if let (Some(app), Some(env)) = (m.value_of("APPLICATION"), m.value_of("ENVIRONMENT")) {
        Ok(Some(ApplicationEnvironment::new(
            app.to_string(),
            env.to_string(),
        )?))
    } else {
        Ok(None)
    }
}

fn get_topology_from_input(m: &ArgMatches) -> Option<Topology> {
    m.value_of("TOPOLOGY").and_then(
        |f| Topology::from_str(f).ok(),
    )
}

fn get_strategy_from_input(m: &ArgMatches) -> Option<UpdateStrategy> {
    m.value_of("STRATEGY").and_then(
        |f| UpdateStrategy::from_str(f).ok(),
    )
}

fn get_binds_from_input(m: &ArgMatches) -> Result<Vec<protocols::types::ServiceBind>> {
    let mut binds = vec![];
    if let Some(bind_strs) = m.values_of("BIND") {
        for bind_str in bind_strs {
            binds.push(ServiceBind::from_str(bind_str)?.into());
        }
    }
    Ok(binds)
}

fn get_config_from_input(m: &ArgMatches) -> Option<PathBuf> {
    if let Some(ref config_from) = m.value_of("CONFIG_DIR") {
        outputln!("");
        outputln!(
            "WARNING: Setting '--config-from' should only be used in development, not production!"
        );
        outputln!("");
        Some(PathBuf::from(config_from))
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn get_password_from_input(m: &ArgMatches) -> Result<Option<String>> {
    if let Some(password) = m.value_of("PASSWORD") {
        Ok(Some(encrypt(password.to_string())?))
    } else {
        Ok(None)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn get_password_from_input(_m: &ArgMatches) -> Result<Option<String>> {
    Ok(None)
}

// CLAP Validation Functions
////////////////////////////////////////////////////////////////////////

fn valid_service_group(val: String) -> result::Result<(), String> {
    match ServiceGroup::validate(&val) {
        Ok(()) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

fn valid_topology(val: String) -> result::Result<(), String> {
    match Topology::from_str(&val) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!("Service topology: '{}' is not valid", &val)),
    }
}

fn valid_listen_gossip(val: String) -> result::Result<(), String> {
    match GossipListenAddr::from_str(&val) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!(
            "Listen gossip address should include both IP and port, eg: '0.0.0.0:9700'"
        )),
    }
}

fn valid_listen_http(val: String) -> result::Result<(), String> {
    match ListenAddr::from_str(&val) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!(
            "Listen http address should include both IP and port, eg: '0.0.0.0:9700'"
        )),
    }
}

fn valid_update_strategy(val: String) -> result::Result<(), String> {
    match UpdateStrategy::from_str(&val) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!("Update strategy: '{}' is not valid", &val)),
    }
}

fn valid_url(val: String) -> result::Result<(), String> {
    match Url::parse(&val) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!("URL: '{}' is not valid", &val)),
    }
}

////////////////////////////////////////////////////////////////////////
fn ctl_secret_key(cfg: &ManagerConfig) -> Result<String> {
    let sup_root = cfg.sup_root();
    let mut secret_key = String::new();
    if !ctl_gateway::read_secret_key(&sup_root, &mut secret_key)? {
        return Err(sup_error!(Error::CtlSecretNotFound(sup_root)));
    }
    Ok(secret_key)
}

fn enable_features_from_env() {
    let features = vec![(feat::List, "LIST")];

    for feature in &features {
        match henv::var(format!("HAB_FEAT_{}", feature.1)) {
            Ok(ref val) if ["true", "TRUE"].contains(&val.as_str()) => {
                feat::enable(feature.0);
                outputln!("Enabling feature: {:?}", feature.0);
            }
            _ => {}
        }
    }

    if feat::is_enabled(feat::List) {
        outputln!("Listing feature flags environment variables:");
        for feature in &features {
            outputln!("     * {:?}: HAB_FEAT_{}=true", feature.0, feature.1);
        }
        outputln!("The Supervisor will start now, enjoy!");
    }
}

fn toggle_verbosity(m: &ArgMatches) {
    if m.is_present("VERBOSE") {
        hcore::output::set_verbose(true);
    }
}

fn toggle_color(m: &ArgMatches) {
    if m.is_present("NO_COLOR") {
        hcore::output::set_no_color(true);
    }
}

fn handle_ctl_reply(reply: SrvMessage) -> result::Result<(), SrvClientError> {
    let mut bar = pbr::ProgressBar::<io::Stdout>::new(0);
    bar.set_units(pbr::Units::Bytes);
    bar.show_tick = true;
    bar.message("    ");
    match reply.message_id() {
        "ConsoleLine" => {
            let m = reply.parse::<protocols::ctl::ConsoleLine>().unwrap();
            print!("{}", m);
        }
        "NetProgress" => {
            let m = reply.parse::<protocols::ctl::NetProgress>().unwrap();
            bar.total = m.get_total();
            if bar.set(m.get_position()) >= m.get_total() {
                bar.finish();
            }
        }
        "NetErr" => {
            let m = reply.parse::<protocols::net::NetErr>().unwrap();
            return Err(SrvClientError::from(m));
        }
        _ => (),
    }
    Ok(())
}

// Based on UI::default_with_env, but taking into account the setting
// of the global color variable.
//
// TODO: Ideally we'd have a unified way of setting color, so this
// function wouldn't be necessary. In the meantime, though, it'll keep
// the scope of change contained.
fn ui() -> UI {
    let coloring = if hcore::output::is_color() {
        Coloring::Auto
    } else {
        Coloring::Never
    };
    let isatty = if env::var(NONINTERACTIVE_ENVVAR)
        .map(|val| val == "1" || val == "true")
        .unwrap_or(false)
    {
        Some(false)
    } else {
        None
    };
    UI::default_with(coloring, isatty)
}

/// Set all fields for an `SvcLoad` message that we can from the given opts. This function
/// populates all *shared* options between `run` and `load`.
fn update_svc_load_from_input(m: &ArgMatches, msg: &mut protocols::ctl::SvcLoad) -> Result<()> {
    msg.set_bldr_url(bldr_url(m));
    msg.set_bldr_channel(channel(m));
    if let Some(app_env) = get_app_env_from_input(m)? {
        msg.set_application_environment(app_env.into());
    }
    msg.set_binds(protobuf::RepeatedField::from_vec(get_binds_from_input(m)?));
    msg.set_specified_binds(m.is_present("BIND"));
    if let Some(config_from) = get_config_from_input(m) {
        msg.set_config_from(config_from.to_string_lossy().into_owned());
    }
    msg.set_force(!m.is_present("FORCE"));
    if let Some(group) = get_group_from_input(m) {
        msg.set_group(group);
    }
    if let Some(svc_encrypted_password) = get_password_from_input(m)? {
        msg.set_svc_encrypted_password(svc_encrypted_password);
    }
    if let Some(topology) = get_topology_from_input(m) {
        msg.set_topology(topology);
    }
    if let Some(update_strategy) = get_strategy_from_input(m) {
        msg.set_update_strategy(update_strategy);
    }
    Ok(())
}
