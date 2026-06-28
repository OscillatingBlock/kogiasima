use clap::{Args, Parser};
use std::{fs, path::PathBuf};

/// Generate a config.json for mini-docker runtime
#[derive(Parser, Debug)]
#[command(name = "mini-docker-config", version = "1.0")]
pub struct Cli {
    #[command(flatten)]
    process: ProcessArgs,

    #[command(flatten)]
    root: RootArgs,

    #[command(flatten)]
    linux: LinuxArgs,

    /// Output path for config.json
    #[arg(long, default_value = "config.json", value_name = "PATH")]
    pub output: PathBuf,
}

#[derive(Args, Debug)]
struct ProcessArgs {
    /// Command to run inside the container (default: /bin/sh)
    /// Example: --args /bin/sh  or  --args "/bin/bash -c 'echo hello'"
    #[arg(
        long = "args",
        value_name = "CMD",
        default_value = "/bin/sh",
        action = clap::ArgAction::Append,   // --args /bin/sh --args --login
    )]
    args: Vec<String>,

    /// Extra environment variables (KEY=VALUE)
    /// Example: --env MY_VAR=hello --env DEBUG=1
    #[arg(
        long = "env",
        value_name = "KEY=VALUE",
        action = clap::ArgAction::Append,
    )]
    extra_env: Vec<String>,

    /// Working directory inside the container
    #[arg(long, default_value = "/", value_name = "PATH")]
    cwd: String,

    /// Hostname inside the container
    #[arg(long, default_value = "mini-docker-isolated", value_name = "NAME")]
    hostname: String,
}

// Root filesystem config
#[derive(Args, Debug)]
struct RootArgs {
    /// Path to the root filesystem
    #[arg(long, default_value = "rootfs", value_name = "PATH")]
    rootfs: String,

    /// Mount root filesystem as read-only
    #[arg(long, default_value_t = false)]
    readonly: bool,
}

//  Linux namespaces + resource limits
#[derive(Args, Debug)]
struct LinuxArgs {
    /// Namespaces to enable (repeatable)
    /// Choices: pid, network, mount, uts, ipc, user
    #[arg(
        long = "namespace",
        value_name = "TYPE",
        default_values = ["pid", "network", "mount", "uts"],
        action = clap::ArgAction::Append,
    )]
    namespaces: Vec<String>,

    /// Max number of PIDs allowed in the container (default: 20)
    #[arg(long, default_value_t = 20, value_name = "N")]
    pid_limit: i64,

    /// Memory limit in bytes (default: 100MB)
    #[arg(long, default_value_t = 104_857_600, value_name = "BYTES")]
    memory_limit: i64,
}

use serde_json::{Value, json};

fn build_config(cli: &Cli) -> Value {
    // Base env vars always included, user's extras appended
    let mut env = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "TERM=xterm".to_string(),
    ];
    env.extend(cli.process.extra_env.clone());

    // Namespaces: turn each string into {"type": "..."}
    let namespaces: Vec<Value> = cli
        .linux
        .namespaces
        .iter()
        .map(|ns| json!({ "type": ns }))
        .collect();

    let args: Vec<String> = cli
        .process
        .args
        .iter()
        .flat_map(|a| a.split_whitespace().map(|s| s.to_string()))
        .collect();

    json!({
        "ociVersion": "1.0.2",
        "process": {
            "args": args,
            "env": env,
            "cwd": cli.process.cwd,
            "hostname": cli.process.hostname,
        },
        "root": {
            "path": cli.root.rootfs,
            "readonly": cli.root.readonly,
        },
        "linux": {
            "namespaces": namespaces,
            "resources": {
                "pids":   { "limit": cli.linux.pid_limit },
                "memory": { "limit": cli.linux.memory_limit },
            }
        }
    })
}

pub fn build_config_file() -> Cli {
    let cli = Cli::parse();
    let config = build_config(&cli);
    let json_str = serde_json::to_string_pretty(&config).unwrap();

    fs::write(&cli.output, &json_str).unwrap_or_else(|e| {
        eprintln!("Failed to write {:?}: {}", cli.output, e);
        std::process::exit(1);
    });

    println!("Written config to {:?}", cli.output);
    cli
}
