use std::{net::Ipv4Addr, time::Duration};

use zed_extension_api::{
    self as zed, DebugAdapterBinary, DebugConfig, DebugRequest, DebugScenario, DebugTaskDefinition,
    StartDebuggingRequestArguments, StartDebuggingRequestArgumentsRequest, TcpArguments,
    TcpArgumentsTemplate, Worktree, serde_json,
};

const ADAPTER_NAME: &str = "probe-rs";

/// DAP proxy that runs on Zed's bundled Node.js. Kept in the extension crate so
/// no external helper binary has to be installed or configured.
const DAP_PROXY_JS: &str = include_str!("../proxy/dap_proxy.js");
const DAP_PROXY_FILE: &str = "zed-probe-rs-dap-proxy.js";
const NODE_WRITE_SCRIPT: &str = "require('fs').writeFileSync(process.argv[1], process.argv[2])";

fn verify_adapter_name(adapter_name: &str) -> Result<(), String> {
    if adapter_name != ADAPTER_NAME {
        Err(format!(
            "Unsupported debug adapter name '{adapter_name}', expected '{ADAPTER_NAME}'"
        ))
    } else {
        Ok(())
    }
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// `std::env::temp_dir()` panics inside a WASI component, so resolve the
/// directory ourselves and fall back to a platform default.
fn proxy_script_path() -> std::path::PathBuf {
    let (os, _) = zed::current_platform();
    let dir = match os {
        zed::Os::Windows => std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .or_else(|_| {
                std::env::var("USERPROFILE").map(|home| format!("{home}\\AppData\\Local\\Temp"))
            })
            .unwrap_or_else(|_| "C:\\Windows\\Temp".to_string()),
        _ => std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string()),
    };

    std::path::PathBuf::from(dir).join(DAP_PROXY_FILE)
}

/// Writes the embedded proxy to a temporary file and returns the Node command
/// that Zed should spawn.
fn node_proxy_command(probe_rs: &str, port: u16) -> Result<(String, Vec<String>), String> {
    let node = zed::node_binary_path()
        .map_err(|err| format!("Zed's bundled Node.js is unavailable: {err}"))?;

    let script_path = proxy_script_path().to_string_lossy().into_owned();

    let output = zed::process::Command::new(&node)
        .args(["-e", NODE_WRITE_SCRIPT, script_path.as_str(), DAP_PROXY_JS])
        .output()
        .map_err(|err| format!("failed to write the embedded DAP proxy: {err}"))?;

    if output.status != Some(0) {
        return Err(format!(
            "failed to write the embedded DAP proxy: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok((
        node,
        vec![
            script_path,
            "--port".to_string(),
            port.to_string(),
            "--upstream".to_string(),
            probe_rs.to_string(),
        ],
    ))
}

struct ProbeRsDebugger {
    // ... state
}

impl zed::Extension for ProbeRsDebugger {
    fn new() -> Self
    where
        Self: Sized,
    {
        println!("Creating new instance of the probe-rs-debugger extension");
        Self {}
    }

    fn get_dap_binary(
        &mut self,
        adapter_name: String,
        config: DebugTaskDefinition,
        user_provided_debug_adapter_path: Option<String>,
        worktree: &Worktree,
    ) -> Result<DebugAdapterBinary, String> {
        verify_adapter_name(&adapter_name)?;

        let json_config: serde_json::Value = serde_json::from_str(&config.config)
            .map_err(|err| format!("Failed to parse JSON config: {}", err))?;

        // Keep the "start debugging" request kind in sync with the scenario.
        let request = match json_config.get("request").and_then(|value| value.as_str()) {
            Some("attach") => StartDebuggingRequestArgumentsRequest::Attach,
            _ => StartDebuggingRequestArgumentsRequest::Launch,
        };

        // An explicit `server` field means: connect to an already running
        // `probe-rs dap-server` instead of spawning one.
        if let Some(server_string) = json_config.get("server").and_then(|value| value.as_str()) {
            let mut connection = parse_server_string(server_string)?;
            // This timeout is in milliseconds.
            connection.timeout = Some(DEFAULT_TIMEOUT.as_millis() as u64);

            return Ok(DebugAdapterBinary {
                command: None,
                arguments: Vec::new(),
                envs: Vec::new(),
                cwd: None,
                connection: Some(connection),
                request_args: StartDebuggingRequestArguments {
                    configuration: config.config,
                    request,
                },
            });
        }

        // `dap.probe-rs.binary` overrides the probe-rs binary itself; the DAP
        // proxy is always provided by this extension.
        let probe_rs = user_provided_debug_adapter_path
            .or_else(|| worktree.which("probe-rs"))
            .unwrap_or_else(|| "probe-rs".to_string());

        // Let Zed allocate a free loopback port. The upstream extension
        // hardcoded 50000, which left stale dap-server processes behind and
        // made a second debug session fail to bind.
        let connection = zed::resolve_tcp_template(TcpArgumentsTemplate {
            port: None,
            host: Some(Ipv4Addr::LOCALHOST.to_bits()),
            timeout: Some(DEFAULT_TIMEOUT.as_millis() as u64),
        })
        .map_err(|err| format!("Failed to allocate a TCP port for the DAP session: {err}"))?;

        // Prefer the embedded proxy: it performs the RTT handshake and turns
        // RTT data into standard DAP output events. If Zed's Node runtime is
        // unavailable, fall back to probe-rs directly (debugging still works,
        // but RTT logs do not).
        println!(
            "probe-rs extension: embedded DAP proxy for {probe_rs} on 127.0.0.1:{}",
            connection.port
        );
        match node_proxy_command(&probe_rs, connection.port) {
            Ok((node, arguments)) => Ok(DebugAdapterBinary {
                command: Some(node),
                arguments,
                envs: Vec::new(),
                cwd: None,
                connection: Some(connection),
                request_args: StartDebuggingRequestArguments {
                    configuration: config.config,
                    request,
                },
            }),
            Err(error) => {
                eprintln!("probe-rs extension: {error}; RTT logs will be unavailable");
                Ok(DebugAdapterBinary {
                    command: Some(probe_rs),
                    arguments: vec![
                        "dap-server".to_string(),
                        "--port".to_string(),
                        connection.port.to_string(),
                        // Probe-rs otherwise keeps listening after the client
                        // disconnects, holding both the TCP port and the USB probe.
                        "--single-session".to_string(),
                    ],
                    envs: Vec::new(),
                    cwd: None,
                    connection: Some(connection),
                    request_args: StartDebuggingRequestArguments {
                        configuration: config.config,
                        request,
                    },
                })
            }
        }
    }

    fn dap_request_kind(
        &mut self,
        adapter_name: String,
        config: serde_json::Value,
    ) -> Result<StartDebuggingRequestArgumentsRequest, String> {
        verify_adapter_name(&adapter_name)?;

        // There should be a request field to indicate if it should be launch or attach
        let Some(request_value) = config.get("request").and_then(|f| f.as_str()) else {
            return Err("Missing 'request' field in configuration".to_string());
        };

        match request_value {
            "launch" => Ok(StartDebuggingRequestArgumentsRequest::Launch),
            "attach" => Ok(StartDebuggingRequestArgumentsRequest::Attach),
            _ => Err(format!(
                "Invalid value for the 'request' field in configuration. Value is {}, but only 'launch' and 'attach' are supported",
                request_value
            )),
        }
    }

    fn dap_config_to_scenario(
        &mut self,
        debug_config: DebugConfig,
    ) -> Result<DebugScenario, String> {
        verify_adapter_name(&debug_config.adapter)?;

        match debug_config.request {
            DebugRequest::Launch(launch_request) => {
                if !launch_request.args.is_empty() {
                    return Err(
                        "Passing arguments is not supported by this debug adapter".to_string()
                    );
                }

                if !launch_request.envs.is_empty() {
                    return Err(
                        "Setting environment variables is not supported by this debug adapter"
                            .to_string(),
                    );
                }

                // We only get a single program, so we can't create a configuration which would
                // work in a multi-core scenario.
                //
                // We also enable flashing to mimic launching a program.
                let config = serde_json::json!({
                    "cwd": launch_request.cwd,
                    "coreConfigs": [
                        {
                            "programBinary": launch_request.program
                        }
                    ],
                    "flashingConfig": {
                        "flashingEnabled": true,
                        "haltAfterReset": debug_config.stop_on_entry,
                    },
                    "request": "launch",

                });

                let scenario = DebugScenario {
                    label: debug_config.label,
                    adapter: debug_config.adapter,
                    // TODO: Could integrate with cargo
                    build: None,
                    config: config.to_string(),
                    tcp_connection: None,
                };

                Ok(scenario)
            }
            DebugRequest::Attach(_attach_request) => {
                // We can't really support attach in the traditional sense, because we can't attach to a running program on the
                // host
                Err("Attaching to a process is not supported by this debug adapter".to_string())
            }
        }
    }
}

fn parse_server_string(server_string: &str) -> Result<TcpArguments, String> {
    // The README used to contain a trailing space in the example, which made
    // the old parser reject an otherwise valid value.
    let server_string = server_string.trim();
    let parts: Vec<&str> = server_string.split(':').collect();

    if parts.len() != 2 {
        return Err(format!(
            "Invalid server string format '{}'. Expected format: 'host:port'",
            server_string
        ));
    }

    let host_str = parts[0];
    let port_str = parts[1];

    let host_ip: Ipv4Addr = if host_str.eq_ignore_ascii_case("localhost") {
        Ipv4Addr::LOCALHOST
    } else {
        host_str.parse().map_err(|_| {
            format!(
                "Invalid IP address '{}'. Expected a valid IPv4 address",
                host_str
            )
        })?
    };

    let port: u16 = port_str.parse().map_err(|_| {
        format!(
            "Invalid port number '{}'. Expected a number between 0 and 65535",
            port_str
        )
    })?;

    Ok(TcpArguments {
        port,
        host: host_ip.to_bits(),
        timeout: None,
    })
}

zed::register_extension!(ProbeRsDebugger);

#[cfg(test)]
mod test {
    use std::net::Ipv4Addr;

    #[test]
    fn parse_server_string_invalid_format() {
        // Test missing port
        let result = super::parse_server_string("127.0.0.1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid server string format"));

        // Test too many colons
        let result = super::parse_server_string("127.0.0.1:3000:extra");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid server string format"));

        // Test empty string
        let result = super::parse_server_string("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid server string format"));
    }

    #[test]
    fn parse_server_string_invalid_ip() {
        // Test invalid IP address
        let result = super::parse_server_string("999.999.999.999:3000");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid IP address"));

        // Test non-IP host
        let result = super::parse_server_string("localhost:3000");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid IP address"));

        // Test empty host
        let result = super::parse_server_string(":3000");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid IP address"));
    }

    #[test]
    fn parse_server_string_invalid_port() {
        // Test non-numeric port
        let result = super::parse_server_string("127.0.0.1:abc");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid port number"));

        // Test port too large
        let result = super::parse_server_string("127.0.0.1:70000");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid port number"));

        // Test negative port (this will fail parsing as u16)
        let result = super::parse_server_string("127.0.0.1:-1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid port number"));

        // Test empty port
        let result = super::parse_server_string("127.0.0.1:");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid port number"));
    }

    #[test]
    fn parse_server_string_trims_and_accepts_localhost() {
        let result = super::parse_server_string(" localhost:50000 ").unwrap();
        assert_eq!(result.host, Ipv4Addr::LOCALHOST.to_bits());
        assert_eq!(result.port, 50000);
    }

    #[test]
    fn parse_server_string_valid_cases() {
        // Test different valid IP addresses and ports
        let test_cases = [
            ("0.0.0.0:80", Ipv4Addr::new(0, 0, 0, 0), 80),
            ("192.168.1.1:8080", Ipv4Addr::new(192, 168, 1, 1), 8080),
            (
                "255.255.255.255:65535",
                Ipv4Addr::new(255, 255, 255, 255),
                65535,
            ),
            ("10.0.0.1:1", Ipv4Addr::new(10, 0, 0, 1), 1),
        ];

        for (server_string, expected_host, expected_port) in test_cases {
            let result = super::parse_server_string(server_string).unwrap();

            assert_eq!(result.port, expected_port);
            assert_eq!(result.host, expected_host.to_bits());
            assert_eq!(result.timeout, None);
        }
    }
}
