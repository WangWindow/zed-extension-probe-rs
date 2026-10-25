# probe-rs debugger extension for Zed

Debug adapter for [probe-rs](https://probe.rs). Target-agnostic: works with any
chip probe-rs supports (STM32, ESP32, nRF, RP2040, ...).

## Configuration

`.zed/debug.json`:

```json
[
  {
    "label": "Launch probe-rs debugging",
    "adapter": "probe-rs",
    "request": "launch",
    "cwd": "$ZED_WORKTREE_ROOT",
    "chip": "<chip-name-from-probe-rs-chip-list>",
    "flashingConfig": { "flashingEnabled": true },
    "coreConfigs": [
      {
        "coreIndex": 0,
        "programBinary": "<path-to-firmware.elf>",
        "rttEnabled": true
      }
    ]
  }
]
```

Find the chip name with `probe-rs chip list`. Remaining options match the
[probe-rs debugger docs](https://probe.rs/docs/tools/debugger/).

## RTT / defmt

Zed does not answer probe-rs' `probe-rs-rtt-channel-config` event, so probe-rs
never forwards RTT data. The extension embeds `proxy/dap_proxy.js`, runs it with
Zed's bundled Node.js, performs the `rttWindowOpened` handshake, and converts
`probe-rs-rtt-data` into standard DAP `output` events.

## Development

```sh
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/zed_probe_rs.wasm extension.wasm
```

Then `zed: install dev extension` or `zed: reload extensions`.

The extension declares the `process:exec` capability to run Zed's bundled Node.
