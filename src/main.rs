#![doc = include_str!("../README.md")]

use anyhow::Context;
use serde::Serialize;
use std::{collections::HashMap, process::Stdio};
use tokio::{io::AsyncBufReadExt, process::Command};

use crate::schema::{msvc, probe_rs};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run().await
}

mod schema {
    pub mod probe_rs {
        use serde::{Deserialize, Serialize};

        #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct Config {
            pub r#type: String,
            pub request: String,
            pub flashing_config: FlashingConfig,
            pub chip: String,
            pub core_configs: Vec<CoreConfig>,
        }

        #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct FlashingConfig {
            pub flashing_enabled: bool,
        }

        #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct CoreConfig {
            pub program_binary: String,
            pub core_index: u8,
            pub rtt_enabled: bool,
        }
    }

    pub mod msvc {
        use serde::{Deserialize, Serialize};
        use std::collections::HashMap;

        #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct Config {
            pub r#type: String,
            pub request: String,
            pub program: String,
            pub args: Vec<String>,
            pub stop_at_entry: bool,
            pub cwd: String,
            pub environment: HashMap<String, String>,
        }
    }
}

fn code_cmd() -> Command {
    tokio::process::Command::new(if cfg!(target_os = "windows") {
        "code.cmd"
    } else {
        "code"
    })
}

pub async fn launch<T>(config: &T) -> anyhow::Result<()>
where
    T: ?Sized + Serialize,
{
    let config = serde_json::ser::to_string(config)?;
    let config = url_escape::encode_query(&config);

    let url = format!(
        "vscode://fabiospampinato.vscode-debug-launcher/launch?args={config}",
        config = config,
    );

    code_cmd()
        .args(["--open-url", &url])
        .output()
        .await
        .context("Failed to launch code. The extension may need to be installed with `cargo debug --install`")?;

    Ok(())
}

/// Run any of your workspace's binaries with the debugger attached.
///
/// cdb serve
async fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let args = if let Some(arg) = args.get(1)
        && arg == "debug"
    {
        &args[1..]
    } else {
        &args[..]
    };

    let mut parsing_cargo_args = true;
    let mut parsing_env_args = true;
    let mut parsing_chip_arg = false;
    let mut cargo_args = vec![];
    let mut process_env_args = vec![];
    let mut rest_args = vec![];
    let mut chip: Option<String> = None;

    for arg in args.iter().skip(1) {
        // Switch to parsing the rest of the args
        if arg == "--" && parsing_cargo_args {
            parsing_cargo_args = false;
            continue;
        }

        if arg.starts_with("-") {
            parsing_chip_arg = false;
        }

        if parsing_chip_arg {
            chip = Some(arg.clone());
            parsing_chip_arg = false;
            continue;
        }

        if arg == "--chip" {
            chip = Some("".to_string());
            parsing_chip_arg = true;
            continue;
        }

        // Attempt to parse env pairs
        if parsing_env_args && !parsing_cargo_args {
            if let Some((left, right)) = arg.split_once('=') {
                if left.is_empty() || right.is_empty() {
                    return Err(anyhow::anyhow!("Invalid argument: {}", arg));
                }

                process_env_args.push((left.to_string(), right.to_string()));
            } else {
                parsing_env_args = false;
                rest_args.push(arg.to_string());
            }
            continue;
        }

        // Attempt to parse cargo args
        if parsing_cargo_args {
            cargo_args.push(arg.to_string());
            continue;
        }

        // Attempt to parse rest args
        rest_args.push(arg.to_string());
    }

    if cargo_args.iter().any(|arg| arg == "--help") {
        println!("cargo debug [cargo args] -- [env1=val1 env2=val2] [executable args]");

        return Ok(());
    }

    // println!("cargo args: {:?}", cargo_args);
    // println!("rest args: {:?}", rest_args);

    let mut child = tokio::process::Command::new("cargo")
        .arg("rustc")
        .args(cargo_args)
        .arg("--message-format")
        .arg("json-diagnostic-rendered-ansi")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn cargo process")?;

    let stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let stderr = tokio::io::BufReader::new(child.stderr.take().unwrap());
    let mut output_location = None;
    let mut stdout = stdout.lines();
    let mut stderr = stderr.lines();

    loop {
        use cargo_metadata::Message;

        let line = tokio::select! {
            Ok(Some(line)) = stdout.next_line() => line,
            Ok(Some(line)) = stderr.next_line() => line,
            else => break,
        };

        let Some(Ok(message)) = Message::parse_stream(std::io::Cursor::new(line)).next() else {
            continue;
        };

        match message {
            Message::CompilerArtifact(artifact) => {
                if let Some(i) = artifact.executable {
                    output_location = Some(i)
                }
            }
            Message::CompilerMessage(compiler_message) => {
                if let Some(rendered) = compiler_message.message.rendered {
                    println!("{rendered}");
                }
            }
            Message::BuildScriptExecuted(_build_script) => {}
            Message::BuildFinished(build_finished) => {
                if !build_finished.success {
                    // assuming we received a message from the compiler, so we can exit
                    return Ok(());
                }
            }
            Message::TextLine(word) => println!("{word}"),
            _ => {}
        }
    }

    let output_location =
        output_location.context("Failed to find output location. Build must've failed.")?;

    if let Some(chip) = chip {
        let config = probe_rs::Config {
            r#type: "probe-rs-debug".to_string(),
            request: "launch".to_string(),
            flashing_config: probe_rs::FlashingConfig {
                flashing_enabled: true,
            },
            chip: chip,
            core_configs: vec![probe_rs::CoreConfig {
                program_binary: output_location.to_string(),
                core_index: 0,
                rtt_enabled: true,
            }],
        };

        launch(&config).await?;
    } else {
        // TODO: launch another debugger type on unix

        let config = msvc::Config {
            r#type: "cppvsdbg".to_string(),
            request: "launch".to_string(),
            program: output_location.to_string(),
            args: rest_args,
            stop_at_entry: false,
            cwd: "${workspaceRoot}".to_string(),
            environment: HashMap::new(),
        };

        launch(&config).await?;
    }

    Ok(())
}
