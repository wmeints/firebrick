use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::builder::PossibleValuesParser;
use clap::{Args, Parser, Subcommand};
use firebrick_cli::api::sandbox_management_service_client::SandboxManagementServiceClient;
use firebrick_cli::network::presets;
use firebrick_cli::network::{self, NetworkChange};
use firebrick_cli::remote::{session, ssh};
use firebrick_cli::sandbox::OutputFormat;
use firebrick_cli::secret::Scope;
use firebrick_cli::spec_file::{self, init, validate};
use firebrick_cli::{client, port, sandbox, secret};
use firebrick_spec::PortMapping;
use tonic::transport::Channel;

/// Firebrick - Run coding agents safely in a sandbox.
#[derive(Parser, Debug)]
#[command(name = "fbk", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start a sandbox
    Start {
        /// Name of the sandbox as listed by `fbk ls`. Defaults to the sandbox for the working directory
        name: Option<String>,

        /// Apply changed mounts and network settings in .firebrick.yml without asking
        #[arg(long, short)]
        yes: bool,
    },
    /// Stop a running sandbox
    Stop {
        /// Name of the sandbox as listed by `fbk ls`. Defaults to the sandbox for the working directory
        name: Option<String>,
    },
    /// List all sandboxes
    Ls {
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
    /// Remove a sandbox
    Rm {
        /// Name of the sandbox as listed by `fbk ls`. Defaults to the sandbox for the working directory
        name: Option<String>,

        /// Stop the sandbox first when it's running
        #[arg(long)]
        force: bool,
    },
    /// Recreate a sandbox from the latest version of its image. It keeps its settings, secrets,
    /// network rules and ports, but not its installed packages or Docker data
    Upgrade {
        /// Name of the sandbox as listed by `fbk ls`. Defaults to the sandbox for the working directory
        name: Option<String>,
    },
    /// Run a command inside the sandbox
    Run(RunArgs),
    /// Validate the .firebrick.yml file in the working directory
    Validate,
    /// Write a .firebrick.yml with the default settings to the working directory
    Init {
        /// Overwrite an existing .firebrick.yml
        #[arg(long)]
        force: bool,
    },
    /// Manage the secrets sandboxes use without seeing their values
    #[command(subcommand)]
    Secret(SecretCommands),
    /// Change the network settings in .firebrick.yml and apply them to the sandbox, or show the
    /// sandbox's network activity
    #[command(subcommand)]
    Network(NetworkCommands),
    /// Forward host ports to the working directory's sandbox and record them in .firebrick.yml
    #[command(subcommand)]
    Port(PortCommands),
    /// Tunnel an SSH connection to a sandbox over stdin/stdout (used by the generated SSH config)
    #[command(hide = true)]
    SshProxy {
        /// Host name of the sandbox, e.g. `project.fbk`
        hostname: String,
    },
}

#[derive(Subcommand, Debug)]
enum SecretCommands {
    /// Set a secret for all sandboxes, or for the working directory's sandbox only
    Set(SetSecretArgs),
    /// List the secrets of all scopes and their allowed hosts, without their values
    Ls {
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
    /// Remove a secret from all sandboxes, or from the working directory's sandbox only
    Rm {
        /// Name of the secret, e.g. GH_TOKEN
        name: String,

        /// Remove the global secret, or the working directory's sandbox-scoped secret
        #[arg(long, value_enum, default_value_t = Scope::Global)]
        scope: Scope,
    },
}

/// The `--yes` flag of the `fbk network` subcommands that change the network settings.
#[derive(Args, Debug, Default)]
struct ApproveArgs {
    /// Apply unapproved network settings in .firebrick.yml without asking
    #[arg(long, short)]
    yes: bool,
}

#[derive(Subcommand, Debug)]
enum NetworkCommands {
    /// Allow the sandbox to connect to destinations, and stop denying them
    Allow {
        /// Host name, *.domain, IP address or CIDR range, e.g. example.org or *.npmjs.org
        #[arg(required = true, value_name = "RULE")]
        rules: Vec<String>,

        #[command(flatten)]
        approve: ApproveArgs,
    },
    /// Deny the sandbox to connect to destinations, and stop allowing them
    Deny {
        /// Host name, *.domain, IP address or CIDR range, e.g. example.org or 10.0.0.0/8
        #[arg(required = true, value_name = "RULE")]
        rules: Vec<String>,

        #[command(flatten)]
        approve: ApproveArgs,
    },
    /// Turn enforcement of the network rules on or off
    #[command(subcommand)]
    Policy(PolicyCommands),
    /// Give the sandbox a network device again
    Enable(ApproveArgs),
    /// Remove the sandbox's network device, so it works fully offline
    Disable(ApproveArgs),
    /// Show the domains and IP addresses the sandbox contacted, newest first. The record comes
    /// from inside the sandbox, so an agent can tamper with it
    Log,
    /// Show or apply the built-in groups of allow rules for coding agents and toolchains
    #[command(subcommand)]
    Presets(PresetCommands),
}

#[derive(Subcommand, Debug)]
enum PresetCommands {
    /// List every preset and its rules
    Show,
    /// Allow the rules of the presets, like `fbk network allow` with those rules
    Apply {
        /// Name of a preset, or `all` for every preset
        #[arg(
            required = true,
            value_name = "PRESET",
            value_parser = PossibleValuesParser::new(presets::names())
        )]
        presets: Vec<String>,

        #[command(flatten)]
        approve: ApproveArgs,
    },
}

#[derive(Subcommand, Debug)]
enum PolicyCommands {
    /// Deny outgoing traffic unless a rule allows it
    Enable(ApproveArgs),
    /// Allow all outgoing traffic; the rules are kept but not enforced
    Disable(ApproveArgs),
}

#[derive(Subcommand, Debug)]
enum PortCommands {
    /// Forward a host port on localhost to a port in the sandbox, right away when it runs
    Forward {
        /// `<port>` to forward localhost:<port> to the same port in the sandbox, or
        /// `<host>:<guest>` to forward localhost:<host> to sandbox port <guest>
        #[arg(value_name = "PORT")]
        port: PortMapping,
    },
    /// Stop forwarding a host port
    Rm {
        /// Host port of the forward, e.g. 8080
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
    },
}

#[derive(Args, Debug)]
struct SetSecretArgs {
    /// Environment variable that exposes the secret in sandboxes, e.g. GH_TOKEN
    name: String,

    /// Value of the secret. Prefer --from-stdin to keep it out of your shell history
    #[arg(required_unless_present = "from_stdin", conflicts_with = "from_stdin")]
    value: Option<String>,

    /// Read the value from stdin
    #[arg(long)]
    from_stdin: bool,

    /// Host that may receive the value, e.g. api.example.com or *.example.com; repeat for more
    /// hosts. Defaults to the hosts of well-known secrets such as GH_TOKEN and ANTHROPIC_API_KEY
    #[arg(long = "allow-host", value_name = "HOST")]
    allowed_hosts: Vec<String>,

    /// Set the secret for all sandboxes, or for the working directory's sandbox only, where it
    /// overrides a global secret with the same name
    #[arg(long, value_enum, default_value_t = Scope::Global)]
    scope: Scope,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Apply changed mounts and network settings in .firebrick.yml without asking when the
    /// sandbox is created
    #[arg(long, short)]
    yes: bool,

    /// Command to execute in the session, followed by its arguments. Options after the command
    /// go to the command, so `fbk run claude --yes` doesn't approve anything
    #[arg(
        required = true,
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let working_dir = working_dir()?;

    match cli.command {
        // Validating and creating the spec don't need the daemon.
        Commands::Validate => validate_spec(&working_dir),
        Commands::Init { force } => init_spec(&working_dir, force),
        Commands::Secret(SecretCommands::Set(args)) => set_secret(args, &working_dir).await,
        // Changes the spec file first and only needs the daemon to apply it.
        Commands::Network(command) => run_network_command(command, &working_dir).await,
        Commands::Upgrade { name } => {
            sandbox::upgrade_sandbox(name, &working_dir, &mut client::connect().await?).await
        }
        command => run_with_daemon(command, working_dir).await,
    }
}

/// Returns the working directory.
#[cfg(unix)]
fn working_dir() -> Result<PathBuf> {
    Ok(env::current_dir()?)
}

/// Returns the working directory in its canonical form. Windows paths are case-insensitive and
/// keep the case they were typed in, so without this `C:\Proj` and `c:\proj` would get sandboxes
/// of their own.
#[cfg(windows)]
fn working_dir() -> Result<PathBuf> {
    Ok(dunce::canonicalize(env::current_dir()?)?)
}

/// Validates the spec in the working directory and exits with code 1 when it is invalid.
fn validate_spec(working_dir: &Path) -> Result<()> {
    if !validate::validate_spec(working_dir)? {
        std::process::exit(1);
    }

    Ok(())
}

/// Writes the default spec to the working directory.
fn init_spec(working_dir: &Path, force: bool) -> Result<()> {
    init::init_spec(working_dir, force)?;
    println!("Created {}", spec_file::SPEC_FILE_NAME);

    Ok(())
}

/// Stores a secret, reading its value before connecting so a failing read doesn't start the
/// daemon.
async fn set_secret(args: SetSecretArgs, working_dir: &Path) -> Result<()> {
    let value = secret_value(&args)?;
    let mut client_instance = client::connect().await?;

    let new_secret = secret::NewSecret {
        name: args.name,
        value,
        allowed_hosts: args.allowed_hosts,
        scope: args.scope,
    };

    secret::set(new_secret, working_dir, &mut client_instance).await
}

/// Runs a command that needs the daemon.
async fn run_with_daemon(command: Commands, working_dir: PathBuf) -> Result<()> {
    let mut client_instance = client::connect().await?;
    let client_instance = &mut client_instance;

    match command {
        Commands::Start { name, yes } => {
            sandbox::start_sandbox(name, &working_dir, yes, client_instance).await
        }
        Commands::Stop { name } => sandbox::stop_sandbox(name, &working_dir, client_instance).await,
        Commands::Ls { format } => sandbox::list_sandboxes(client_instance, format).await,
        Commands::Rm { name, force } => {
            sandbox::remove_sandbox(name, force, &working_dir, client_instance).await
        }
        Commands::Run(run_args) => run_command(working_dir, run_args, client_instance).await,
        Commands::SshProxy { hostname } => ssh_proxy(hostname, client_instance).await,
        Commands::Secret(SecretCommands::Ls { format }) => {
            secret::list(format, client_instance).await
        }
        Commands::Secret(SecretCommands::Rm { name, scope }) => {
            secret::remove(name, scope, &working_dir, client_instance).await
        }
        Commands::Port(command) => run_port_command(command, &working_dir, client_instance).await,
        Commands::Validate
        | Commands::Init { .. }
        | Commands::Secret(SecretCommands::Set(_))
        | Commands::Network(_)
        | Commands::Upgrade { .. } => {
            unreachable!("handled before connecting to the daemon")
        }
    }
}

/// Changes a port forward of the working directory's sandbox.
async fn run_port_command(
    command: PortCommands,
    working_dir: &Path,
    client_instance: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    match command {
        PortCommands::Forward { port } => port::forward(port, working_dir, client_instance).await,
        PortCommands::Rm { port } => port::remove(port, working_dir, client_instance).await,
    }
}

/// Runs a command in the sandbox attached to the terminal and exits with its exit code.
async fn run_command(
    working_dir: PathBuf,
    run_args: RunArgs,
    client_instance: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let code =
        session::attach(working_dir, run_args.command, run_args.yes, client_instance).await?;

    std::process::exit(code);
}

/// Proxies an SSH connection to the sandbox with the host name, then exits.
async fn ssh_proxy(
    hostname: String,
    client_instance: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    ssh::proxy(hostname, client_instance).await?;

    // Exit right away rather than wait for the stdin thread, which blocks on a read.
    std::process::exit(0);
}

/// What an `fbk network` subcommand does.
#[derive(Debug, PartialEq, Eq)]
enum NetworkAction {
    /// Change the network section of the spec, approved without asking when the flag is set.
    Change(NetworkChange, bool),
    /// Show the sandbox's network activity.
    Log,
    /// Show the presets, which reads neither the spec nor the daemon.
    ShowPresets,
}

/// Runs an `fbk network` subcommand in the working directory.
async fn run_network_command(command: NetworkCommands, working_dir: &Path) -> Result<()> {
    match network_action(command)? {
        NetworkAction::Change(change, yes) => network::update(change, working_dir, yes).await,
        NetworkAction::Log => network::log(working_dir, &mut client::connect().await?).await,
        NetworkAction::ShowPresets => {
            print!("{}", presets::show()?);
            Ok(())
        }
    }
}

/// Returns what the subcommand does: for a change to the network section of the spec, the
/// change with its rules validated and whether `--yes` approves it.
fn network_action(command: NetworkCommands) -> Result<NetworkAction> {
    let (change, yes) = match command {
        NetworkCommands::Allow { rules, approve } => (NetworkChange::allow(&rules)?, approve.yes),
        NetworkCommands::Presets(PresetCommands::Apply { presets, approve }) => {
            (NetworkChange::Allow(presets::rules(&presets)?), approve.yes)
        }
        NetworkCommands::Deny { rules, approve } => (NetworkChange::deny(&rules)?, approve.yes),
        NetworkCommands::Policy(PolicyCommands::Enable(approve)) => {
            (NetworkChange::Enforce(true), approve.yes)
        }
        NetworkCommands::Policy(PolicyCommands::Disable(approve)) => {
            (NetworkChange::Enforce(false), approve.yes)
        }
        NetworkCommands::Enable(approve) => (NetworkChange::Enable(true), approve.yes),
        NetworkCommands::Disable(approve) => (NetworkChange::Enable(false), approve.yes),
        NetworkCommands::Log => return Ok(NetworkAction::Log),
        NetworkCommands::Presets(PresetCommands::Show) => return Ok(NetworkAction::ShowPresets),
    };

    Ok(NetworkAction::Change(change, yes))
}

/// Returns the secret value from the arguments, or from stdin with `--from-stdin`.
fn secret_value(args: &SetSecretArgs) -> Result<String> {
    match &args.value {
        Some(value) => Ok(value.clone()),
        None => secret::read_value(std::io::stdin().lock()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse_secret_set(args: &[&str]) -> Result<SetSecretArgs, clap::Error> {
        let cli = Cli::try_parse_from(["fbk", "secret", "set"].iter().chain(args))?;

        match cli.command {
            Commands::Secret(SecretCommands::Set(args)) => Ok(args),
            command => panic!("unexpected command {command:?}"),
        }
    }

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_prints_package_version() {
        let error = Cli::try_parse_from(["fbk", "--version"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string(),
            format!("fbk {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn start_stop_and_rm_take_an_optional_name() {
        let parse = |args: &[&str]| Cli::try_parse_from(["fbk"].iter().chain(args)).unwrap();

        assert!(matches!(
            parse(&["start"]).command,
            Commands::Start { name: None, .. }
        ));
        assert!(matches!(
            parse(&["stop"]).command,
            Commands::Stop { name: None }
        ));
        assert!(matches!(
            parse(&["rm"]).command,
            Commands::Rm {
                name: None,
                force: false
            }
        ));
        assert!(matches!(
            parse(&["start", "dev"]).command,
            Commands::Start { name: Some(name), .. } if name == "dev"
        ));
        assert!(matches!(
            parse(&["stop", "dev"]).command,
            Commands::Stop { name: Some(name) } if name == "dev"
        ));
        assert!(matches!(
            parse(&["rm", "dev"]).command,
            Commands::Rm { name: Some(name), force: false } if name == "dev"
        ));
    }

    #[test]
    fn upgrade_takes_an_optional_name() {
        let parse = |args: &[&str]| Cli::try_parse_from(["fbk"].iter().chain(args)).unwrap();

        assert!(matches!(
            parse(&["upgrade"]).command,
            Commands::Upgrade { name: None }
        ));
        assert!(matches!(
            parse(&["upgrade", "dev"]).command,
            Commands::Upgrade { name: Some(name) } if name == "dev"
        ));
    }

    #[test]
    fn rm_takes_force_flag() {
        let parse = |args: &[&str]| Cli::try_parse_from(["fbk"].iter().chain(args)).unwrap();

        assert!(matches!(
            parse(&["rm", "--force"]).command,
            Commands::Rm {
                name: None,
                force: true
            }
        ));
        assert!(matches!(
            parse(&["rm", "--force", "dev"]).command,
            Commands::Rm { name: Some(name), force: true } if name == "dev"
        ));
    }

    #[test]
    fn init_takes_force_flag() {
        let parse = |args: &[&str]| Cli::try_parse_from(["fbk"].iter().chain(args)).unwrap();

        assert!(matches!(
            parse(&["init"]).command,
            Commands::Init { force: false }
        ));
        assert!(matches!(
            parse(&["init", "--force"]).command,
            Commands::Init { force: true }
        ));
    }

    #[test]
    fn secret_set_takes_value_and_hosts() {
        let args = parse_secret_set(&[
            "MY_TOKEN",
            "abc",
            "--allow-host",
            "a.example.com",
            "--allow-host",
            "*.example.org",
        ])
        .unwrap();

        assert_eq!(args.name, "MY_TOKEN");
        assert_eq!(args.value.as_deref(), Some("abc"));
        assert!(!args.from_stdin);
        assert_eq!(args.allowed_hosts, ["a.example.com", "*.example.org"]);
    }

    #[test]
    fn secret_set_takes_value_from_stdin() {
        let args = parse_secret_set(&["GH_TOKEN", "--from-stdin"]).unwrap();

        assert!(args.from_stdin);
        assert_eq!(args.value, None);
    }

    #[test]
    fn secret_ls_defaults_to_table() {
        let cli = Cli::try_parse_from(["fbk", "secret", "ls"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Secret(SecretCommands::Ls {
                format: OutputFormat::Table
            })
        ));
    }

    #[test]
    fn secret_rm_requires_name() {
        assert!(Cli::try_parse_from(["fbk", "secret", "rm"]).is_err());

        let cli = Cli::try_parse_from(["fbk", "secret", "rm", "GH_TOKEN"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Secret(SecretCommands::Rm { name, scope: Scope::Global }) if name == "GH_TOKEN"
        ));
    }

    fn parse_secret_rm_scope(args: &[&str]) -> Result<Scope, clap::Error> {
        let cli = Cli::try_parse_from(["fbk", "secret", "rm", "X"].iter().chain(args))?;

        match cli.command {
            Commands::Secret(SecretCommands::Rm { scope, .. }) => Ok(scope),
            command => panic!("unexpected command {command:?}"),
        }
    }

    fn parse_port(args: &[&str]) -> Result<PortCommands, clap::Error> {
        let cli = Cli::try_parse_from(["fbk", "port"].iter().chain(args))?;

        match cli.command {
            Commands::Port(command) => Ok(command),
            command => panic!("unexpected command {command:?}"),
        }
    }

    #[test]
    fn secret_set_and_rm_take_a_scope() {
        let set_scope = |args: &[&str]| {
            let args: Vec<&str> = ["X", "abc"].iter().chain(args).copied().collect();
            parse_secret_set(&args).map(|args| args.scope)
        };

        for parse in [set_scope, parse_secret_rm_scope] {
            assert_eq!(parse(&[]).unwrap(), Scope::Global);
            assert_eq!(parse(&["--scope", "global"]).unwrap(), Scope::Global);
            assert_eq!(parse(&["--scope", "sandbox"]).unwrap(), Scope::Sandbox);
        }
    }

    #[test]
    fn secret_set_and_rm_reject_other_scopes() {
        for result in [
            parse_secret_set(&["X", "abc", "--scope", "project"]).map(|args| args.scope),
            parse_secret_rm_scope(&["--scope", "project"]),
        ] {
            let error = result.unwrap_err();

            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            assert!(error.to_string().contains("global, sandbox"), "{error}");
        }
    }

    #[test]
    fn port_forward_takes_a_port_or_a_mapping() {
        assert!(matches!(
            parse_port(&["forward", "3000"]).unwrap(),
            PortCommands::Forward {
                port: PortMapping {
                    host: 3000,
                    guest: 3000
                }
            }
        ));
        assert!(matches!(
            parse_port(&["forward", "8080:5173"]).unwrap(),
            PortCommands::Forward {
                port: PortMapping {
                    host: 8080,
                    guest: 5173
                }
            }
        ));
    }

    #[test]
    fn port_forward_rejects_invalid_ports() {
        for arg in ["0", "65536", "web", "8080:0", "8080:", ":80", "1:2:3"] {
            assert!(parse_port(&["forward", arg]).is_err(), "{arg}");
        }
        assert!(parse_port(&["forward"]).is_err());
    }

    #[test]
    fn port_rm_takes_a_host_port() {
        assert!(matches!(
            parse_port(&["rm", "8080"]).unwrap(),
            PortCommands::Rm { port: 8080 }
        ));
        for arg in ["0", "65536", "web", "8080:5173"] {
            assert!(parse_port(&["rm", arg]).is_err(), "{arg}");
        }
    }

    #[test]
    fn secret_set_requires_exactly_one_value_source() {
        assert!(parse_secret_set(&["GH_TOKEN"]).is_err());
        assert!(parse_secret_set(&["GH_TOKEN", "abc", "--from-stdin"]).is_err());
    }

    fn parse_network(args: &[&str]) -> Result<NetworkCommands, clap::Error> {
        let cli = Cli::try_parse_from(["fbk", "network"].iter().chain(args))?;

        match cli.command {
            Commands::Network(command) => Ok(command),
            command => panic!("unexpected command {command:?}"),
        }
    }

    #[test]
    fn start_takes_a_yes_flag() {
        let parse = |args: &[&str]| Cli::try_parse_from(["fbk"].iter().chain(args)).unwrap();

        for (args, expected) in [(["start"].as_slice(), false), (&["start", "--yes"], true)] {
            assert!(matches!(
                parse(args).command,
                Commands::Start { yes, .. } if yes == expected
            ));
        }
    }

    #[test]
    fn run_takes_a_yes_flag_only_before_the_command() {
        let parse = |args: &[&str]| Cli::try_parse_from(["fbk"].iter().chain(args)).unwrap();

        assert!(matches!(
            parse(&["run", "-y", "claude", "--yes"]).command,
            Commands::Run(RunArgs { yes: true, command }) if command == ["claude", "--yes"]
        ));
        assert!(matches!(
            parse(&["run", "claude", "-y", "--model", "opus"]).command,
            Commands::Run(RunArgs { yes: false, command }) if command == ["claude", "-y", "--model", "opus"]
        ));
        assert_eq!(
            Cli::try_parse_from(["fbk", "run"]).unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    /// Returns whether `--yes` approves the change of the parsed `fbk network` arguments.
    fn network_yes(args: &[&str]) -> bool {
        match network_action(parse_network(args).unwrap()).unwrap() {
            NetworkAction::Change(_, yes) => yes,
            action => panic!("expected a change, got {action:?}"),
        }
    }

    #[test]
    fn every_network_subcommand_that_changes_settings_takes_a_yes_flag() {
        for args in [
            ["allow", "example.org", "--yes"].as_slice(),
            &["deny", "--yes", "example.org"],
            &["policy", "enable", "--yes"],
            &["policy", "disable", "-y"],
            &["enable", "--yes"],
            &["disable", "-y"],
        ] {
            assert!(network_yes(args), "{args:?}");
        }
        assert!(!network_yes(&["disable"]));
    }

    #[test]
    fn network_log_takes_no_yes_flag() {
        for args in [["log", "--yes"], ["log", "-y"], ["--yes", "log"]] {
            assert_eq!(
                parse_network(&args).unwrap_err().kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{args:?}"
            );
        }
    }

    #[test]
    fn network_allow_and_deny_take_one_or_more_rules() {
        assert!(matches!(
            parse_network(&["allow", "example.org", "*.npmjs.org"]).unwrap(),
            NetworkCommands::Allow { rules, .. } if rules == ["example.org", "*.npmjs.org"]
        ));
        assert!(matches!(
            parse_network(&["deny", "10.0.0.0/8"]).unwrap(),
            NetworkCommands::Deny { rules, .. } if rules == ["10.0.0.0/8"]
        ));
        assert!(parse_network(&["allow"]).is_err());
        assert!(parse_network(&["deny"]).is_err());
    }

    #[test]
    fn network_policy_takes_enable_or_disable() {
        assert!(matches!(
            parse_network(&["policy", "enable"]).unwrap(),
            NetworkCommands::Policy(PolicyCommands::Enable(_))
        ));
        assert!(matches!(
            parse_network(&["policy", "disable"]).unwrap(),
            NetworkCommands::Policy(PolicyCommands::Disable(_))
        ));
        assert!(parse_network(&["policy"]).is_err());
    }

    #[test]
    fn network_takes_enable_and_disable_without_arguments() {
        assert!(matches!(
            parse_network(&["enable"]).unwrap(),
            NetworkCommands::Enable(_)
        ));
        assert!(matches!(
            parse_network(&["disable"]).unwrap(),
            NetworkCommands::Disable(_)
        ));
        assert!(parse_network(&["enable", "example.org"]).is_err());
        assert!(parse_network(&["disable", "example.org"]).is_err());
    }

    #[test]
    fn network_action_maps_the_commands() {
        let change = |args: &[&str]| network_action(parse_network(args).unwrap()).unwrap();

        assert_eq!(
            change(&["allow", "example.org"]),
            NetworkAction::Change(
                NetworkChange::allow(&["example.org".to_string()]).unwrap(),
                false
            )
        );
        assert_eq!(
            change(&["policy", "enable"]),
            NetworkAction::Change(NetworkChange::Enforce(true), false)
        );
        assert_eq!(
            change(&["policy", "disable"]),
            NetworkAction::Change(NetworkChange::Enforce(false), false)
        );
        assert_eq!(
            change(&["enable"]),
            NetworkAction::Change(NetworkChange::Enable(true), false)
        );
        assert_eq!(
            change(&["disable"]),
            NetworkAction::Change(NetworkChange::Enable(false), false)
        );
        assert_eq!(change(&["log"]), NetworkAction::Log);
        assert_eq!(change(&["presets", "show"]), NetworkAction::ShowPresets);
    }

    #[test]
    fn network_log_takes_no_arguments() {
        assert!(matches!(
            parse_network(&["log"]).unwrap(),
            NetworkCommands::Log
        ));
        assert!(parse_network(&["log", "example.org"]).is_err());
    }

    #[test]
    fn network_presets_show_takes_no_arguments() {
        assert!(matches!(
            parse_network(&["presets", "show"]).unwrap(),
            NetworkCommands::Presets(PresetCommands::Show)
        ));
        assert!(parse_network(&["presets", "show", "rust"]).is_err());
    }

    #[test]
    fn network_presets_apply_takes_one_or_more_presets() {
        for (args, names, yes) in [
            (
                ["presets", "apply", "rust", "github"].as_slice(),
                ["rust", "github"].as_slice(),
                false,
            ),
            (
                &["presets", "apply", "rust", "github", "--yes"],
                &["rust", "github"],
                true,
            ),
            (&["presets", "apply", "all"], &["all"], false),
        ] {
            assert!(
                matches!(
                    parse_network(args).unwrap(),
                    NetworkCommands::Presets(PresetCommands::Apply { presets, approve })
                        if presets == names && approve.yes == yes
                ),
                "{args:?}"
            );
        }
    }

    #[test]
    fn network_presets_apply_rejects_a_missing_or_unknown_preset() {
        assert_eq!(
            parse_network(&["presets", "apply"]).unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        for args in [["presets", "apply", "rustt"], ["presets", "apply", "Rust"]] {
            let error = parse_network(&args).unwrap_err();

            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            assert!(
                error.to_string().contains("all, claude, copilot"),
                "{error}"
            );
        }
    }

    #[test]
    fn network_presets_apply_help_lists_every_preset() {
        let help = Cli::command()
            .find_subcommand_mut("network")
            .and_then(|network| network.find_subcommand_mut("presets"))
            .and_then(|presets| presets.find_subcommand_mut("apply"))
            .expect("the apply subcommand")
            .render_long_help()
            .to_string();

        for name in presets::names() {
            assert!(help.contains(name), "{name} missing from:\n{help}");
        }
    }

    /// Returns the action of `fbk network presets apply` with the arguments.
    fn presets_apply_action(args: &[&str]) -> NetworkAction {
        let args: Vec<&str> = ["presets", "apply"].iter().chain(args).copied().collect();

        network_action(parse_network(&args).unwrap()).unwrap()
    }

    /// Returns the change that allows the rules.
    fn allow(rules: impl IntoIterator<Item = &'static str>) -> NetworkChange {
        let rules: Vec<String> = rules.into_iter().map(String::from).collect();

        NetworkChange::allow(&rules).unwrap()
    }

    #[test]
    fn network_action_maps_presets_apply_to_allow_the_preset_rules() {
        let rules = [
            "*.github.com",
            "*.githubusercontent.com",
            "*.githubassets.com",
            "ghcr.io",
            "*.crates.io",
            "*.rust-lang.org",
            "*.rustup.rs",
            "docs.rs",
        ];

        assert_eq!(
            presets_apply_action(&["rust", "github", "-y"]),
            NetworkAction::Change(allow(rules), true)
        );
    }

    #[test]
    fn network_action_maps_presets_apply_all_to_allow_every_preset_rule() {
        let rules = presets::PRESETS
            .iter()
            .flat_map(|preset| preset.rules)
            .copied();

        assert_eq!(
            presets_apply_action(&["all"]),
            NetworkAction::Change(allow(rules), false)
        );
    }

    #[test]
    fn network_action_rejects_invalid_rules() {
        let deny = NetworkCommands::Deny {
            rules: vec!["https://example.org".to_string()],
            approve: ApproveArgs::default(),
        };

        assert!(
            network_action(deny)
                .unwrap_err()
                .to_string()
                .starts_with("invalid network rule \"https://example.org\"")
        );
    }
}
