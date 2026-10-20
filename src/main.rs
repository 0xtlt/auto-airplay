use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use auto_airplay::config::{AppConfig, ConfigStore, InputKind, OutputProtocol};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "auto-airplay", version, about)]
struct Cli {
    /// Configuration file. Defaults to /etc/auto-airplay/config.toml on Linux.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect or validate configuration.
    Config(ConfigArgs),
    /// Discover and configure an audio input.
    Input(InputArgs),
    /// Discover, configure, and test an AirPlay output.
    Output(OutputArgs),
    /// Run the audio bridge in the foreground.
    Run,
    /// Removed: auto-airplay no longer changes the speaker volume.
    #[command(hide = true)]
    Volume {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Check the host and configured devices.
    Doctor,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the current configuration.
    Show {
        /// Print machine-readable JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
struct InputArgs {
    #[command(subcommand)]
    command: InputCommand,
}

#[derive(Debug, Subcommand)]
enum InputCommand {
    /// Select a Bluetooth audio source by MAC address.
    SetBluetooth {
        address: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Pair, trust, connect, and select a Bluetooth source.
    Pair {
        address: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Select an ALSA capture device.
    SetAlsa {
        device: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List Bluetooth audio sources visible to BlueZ.
    List {
        #[arg(long, default_value_t = 8)]
        seconds: u64,
    },
    /// Open the selected input and report its negotiated format.
    Probe,
}

#[derive(Debug, Args)]
struct OutputArgs {
    #[command(subcommand)]
    command: OutputCommand,
}

#[derive(Debug, Subcommand)]
enum OutputCommand {
    /// Select an AirPlay receiver by name, device ID, or IP.
    Set {
        selector: String,
        #[arg(long, value_enum, default_value_t = ProtocolArg::Airplay2)]
        protocol: ProtocolArg,
    },
    /// List AirPlay receivers discovered over mDNS.
    List {
        #[arg(long, default_value_t = 4)]
        seconds: u64,
    },
    /// Play a short audible tone through an AirPlay receiver.
    Test {
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 880.0)]
        frequency: f32,
        #[arg(long, default_value_t = 3.0)]
        duration: f32,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProtocolArg {
    Airplay1,
    Airplay2,
}

impl From<ProtocolArg> for OutputProtocol {
    fn from(value: ProtocolArg) -> Self {
        match value {
            ProtocolArg::Airplay1 => Self::AirPlay1,
            ProtocolArg::Airplay2 => Self::AirPlay2,
        }
    }
}

fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "auto_airplay=info,airplay_client=warn,airplay_client::receiver_events=info,airplay_audio=warn,airplay_core=warn,airplay_crypto=warn,airplay_discovery=warn,airplay_pairing=warn,airplay_rtsp=warn,airplay_timing=warn",
                    )
                }),
        )
        .with_target(false)
        .try_init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(Cli::parse()));
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let store = ConfigStore::new(cli.config.unwrap_or_else(default_config_path));

    match cli.command {
        Command::Config(args) => match args.command {
            ConfigCommand::Show { json } => {
                let config = load_or_create_config(&store)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    println!("{}", toml::to_string_pretty(&config)?);
                }
            }
        },
        Command::Input(args) => match args.command {
            InputCommand::SetBluetooth { address, name } => {
                let address = normalize_bluetooth_address(&address)?;
                let mut config = load_or_create_config(&store)?;
                config.input.kind = InputKind::Bluetooth;
                config.input.bluetooth_address = Some(address.clone());
                config.input.alsa_device = None;
                config.input.display_name = name.clone();
                store.save(&config)?;
                println!(
                    "Bluetooth input set to {} ({address})",
                    name.as_deref().unwrap_or("unnamed device")
                );
            }
            InputCommand::Pair { address, name } => {
                let address = normalize_bluetooth_address(&address)?;
                auto_airplay::platform::pair_bluetooth_input(&address).await?;
                let mut config = load_or_create_config(&store)?;
                config.input.kind = InputKind::Bluetooth;
                config.input.bluetooth_address = Some(address.clone());
                config.input.alsa_device = None;
                config.input.display_name = name.clone();
                store.save(&config)?;
                println!(
                    "Bluetooth input paired and selected: {} ({address})",
                    name.as_deref().unwrap_or("unnamed device")
                );
            }
            InputCommand::SetAlsa { device, name } => {
                let mut config = load_or_create_config(&store)?;
                config.input.kind = InputKind::Alsa;
                config.input.alsa_device = Some(device.clone());
                config.input.bluetooth_address = None;
                config.input.display_name = name;
                store.save(&config)?;
                println!("ALSA input set to {device}");
            }
            InputCommand::List { seconds } => {
                auto_airplay::platform::list_bluetooth_inputs(seconds).await?;
            }
            InputCommand::Probe => {
                let config = load_config(&store).context("configure an input first")?;
                let format =
                    auto_airplay::platform::probe_input(&config.input, &config.audio).await?;
                println!(
                    "Input format: {} Hz, {} channels, {}-bit",
                    format.sample_rate, format.channels, format.bits_per_sample
                );
            }
        },
        Command::Output(args) => match args.command {
            OutputCommand::Set { selector, protocol } => {
                let mut config = load_or_create_config(&store)?;
                config.output.selector = Some(selector.clone());
                config.output.protocol = protocol.into();
                store.save(&config)?;
                println!("AirPlay output set to {selector}");
            }
            OutputCommand::List { seconds } => {
                for output in auto_airplay::output::discover_outputs(seconds).await? {
                    let addresses = output
                        .addresses
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "{}\t{}\t{}\t{}:{}",
                        output.name, output.id, output.model, addresses, output.port
                    );
                }
            }
            OutputCommand::Test {
                target,
                frequency,
                duration,
            } => {
                let config = load_or_create_config(&store)?;
                let selector = target
                    .as_deref()
                    .or(config.output.selector.as_deref())
                    .context("no output selected; run `auto-airplay output set <name>`")?;
                auto_airplay::output::test_output(selector, frequency, duration, &config.output)
                    .await?;
            }
        },
        Command::Run => {
            auto_airplay::daemon::run(store).await?;
        }
        Command::Volume { .. } => {
            bail!(
                "the `volume` command was removed: auto-airplay no longer changes the speaker \
                 volume; adjust it on the speaker, the iPhone or the Home app"
            );
        }
        Command::Doctor => {
            auto_airplay::platform::doctor(&store).await?;
        }
    }
    Ok(())
}

/// Obsolete keys are reported on stderr so `config show --json` stays valid.
/// Not done in `ConfigStore::load`, which the daemon polls every 2 seconds.
fn warn_obsolete_fields(config: &AppConfig) {
    for field in config.obsolete_fields() {
        eprintln!(
            "warning: obsolete configuration field `{field}` ignored: auto-airplay never changes the speaker volume"
        );
    }
}

fn load_config(store: &ConfigStore) -> Result<AppConfig> {
    let config = store.load()?;
    warn_obsolete_fields(&config);
    Ok(config)
}

fn load_or_create_config(store: &ConfigStore) -> Result<AppConfig> {
    let config = store.load_or_create()?;
    warn_obsolete_fields(&config);
    Ok(config)
}

fn normalize_bluetooth_address(address: &str) -> Result<String> {
    let normalized = address.trim().to_ascii_uppercase();
    let valid = normalized.len() == 17
        && normalized
            .split(':')
            .all(|pair| pair.len() == 2 && pair.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid {
        bail!("invalid Bluetooth address `{address}`; expected XX:XX:XX:XX:XX:XX");
    }
    Ok(normalized)
}

fn default_config_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/etc/auto-airplay/config.toml")
    }

    #[cfg(not(target_os = "linux"))]
    {
        directories::ProjectDirs::from("dev", "0xtlt", "auto-airplay")
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .unwrap_or_else(|| PathBuf::from("auto-airplay.toml"))
    }
}
