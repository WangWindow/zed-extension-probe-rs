#!/usr/bin/env node
// DAP proxy between Zed and `probe-rs dap-server`.
//
// Zed does not answer probe-rs' `probe-rs-rtt-channel-config` event, so
// probe-rs never forwards RTT data. This proxy answers the handshake and
// converts `probe-rs-rtt-data` events into standard DAP output events.
//
// It is spawned by the Zed extension with Zed's bundled Node binary:
//   node dap_proxy.js --port <listen-port> --upstream <probe-rs-binary>

"use strict";

const net = require("net");
const { spawn } = require("child_process");

let port = null;
let upstream = "probe-rs";

for (let i = 2; i < process.argv.length; i++) {
  const arg = process.argv[i];
  if (arg === "--port") {
    port = Number.parseInt(process.argv[++i], 10);
  } else if (arg === "--upstream") {
    upstream = process.argv[++i];
  }
}

if (!Number.isInteger(port) || port <= 0 || port > 65535) {
  console.error("dap_proxy: missing or invalid --port");
  process.exit(2);
}

const CLIENT_SEQ_BASE = 2_000_000_000;
const PROXY_SEQ_BASE = 1_000_000_000;
let clientSeq = CLIENT_SEQ_BASE;

function frame(message) {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  return Buffer.concat([
    Buffer.from(`Content-Length: ${body.length}\r\n\r\n`, "ascii"),
    body,
  ]);
}

class FrameReader {
  constructor(stream, onMessage, onError) {
    this.stream = stream;
    this.onMessage = onMessage;
    this.onError = onError;
    this.buffer = Buffer.alloc(0);

    stream.on("data", (chunk) => {
      this.buffer =
        this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
      this.parse();
    });
    stream.on("error", (error) => this.fail(error));
    stream.on("end", () => this.fail(new Error("stream ended")));
  }

  fail(error) {
    if (!this.failed) {
      this.failed = true;
      this.onError(error);
    }
  }

  parse() {
    while (true) {
      const headerEnd = this.buffer.indexOf("\r\n\r\n");
      if (headerEnd < 0) return;

      const header = this.buffer.subarray(0, headerEnd).toString("ascii");
      let length = null;
      for (const line of header.split("\r\n")) {
        const match = /^Content-Length:\s*(\d+)$/i.exec(line.trim());
        if (match) length = Number.parseInt(match[1], 10);
      }
      if (length === null) {
        this.fail(new Error("missing Content-Length"));
        return;
      }

      const bodyStart = headerEnd + 4;
      if (this.buffer.length < bodyStart + length) return;

      const body = this.buffer.subarray(bodyStart, bodyStart + length);
      this.buffer = this.buffer.subarray(bodyStart + length);

      let message;
      try {
        message = JSON.parse(body.toString("utf8"));
      } catch (error) {
        this.fail(error);
        return;
      }
      this.onMessage(message);
    }
  }
}

const server = net.createServer((client) => {
  // DAP frames are small; Nagle would add up to ~40 ms to RTT output.
  client.setNoDelay(true);

  const child = spawn(upstream, ["dap-server"], {
    stdio: ["pipe", "pipe", "inherit"],
  });

  const pendingProxyRequests = new Set();
  const openedChannels = new Set();
  let proxySeq = PROXY_SEQ_BASE;
  let closed = false;

  // Coalesce all RTT chunks parsed in the same event-loop turn into one DAP
  // output event. This keeps the console responsive under bursts without
  // adding measurable latency (setImmediate runs before the next poll).
  let pendingRtt = "";
  let rttFlushScheduled = false;

  const flushRtt = () => {
    rttFlushScheduled = false;
    if (!pendingRtt) return;
    const text = pendingRtt;
    pendingRtt = "";
    sendToClient({
      seq: 0,
      type: "event",
      event: "output",
      body: { category: "console", output: text },
    });
  };

  const sendToChild = (message) => {
    if (child.stdin.writable) child.stdin.write(frame(message));
  };

  const sendToClient = (message) => {
    if (closed || !client.writable) return;
    message.seq = clientSeq++;
    client.write(frame(message));
  };

  const close = () => {
    if (closed) return;
    closed = true;
    try {
      child.stdin.end();
    } catch (_) {}
    setTimeout(() => {
      try {
        child.kill();
      } catch (_) {}
    }, 100).unref();
    try {
      client.end();
    } catch (_) {}
  };

  // Zed -> probe-rs
  new FrameReader(
    client,
    (message) => sendToChild(message),
    () => close(),
  );

  // probe-rs -> Zed
  new FrameReader(
    child.stdout,
    (message) => {
      if (
        message.type === "response" &&
        pendingProxyRequests.delete(message.request_seq)
      ) {
        return;
      }

      if (message.type === "event") {
        switch (message.event) {
          case "probe-rs-rtt-channel-config": {
            const channel = message.body?.channelNumber ?? 0;
            if (!openedChannels.has(channel)) {
              openedChannels.add(channel);
              const seq = proxySeq++;
              pendingProxyRequests.add(seq);
              sendToChild({
                seq,
                type: "request",
                command: "rttWindowOpened",
                arguments: { channelNumber: channel, windowIsOpen: true },
              });
            }
            return;
          }

          case "probe-rs-rtt-data": {
            const channel = message.body?.channelNumber ?? 0;
            let text = message.body?.data ?? "";
            if (channel !== 0) text = `[RTT ch${channel}] ${text}`;
            if (!text.endsWith("\n")) text += "\n";
            pendingRtt += text;
            if (!rttFlushScheduled) {
              rttFlushScheduled = true;
              setImmediate(flushRtt);
            }
            return;
          }

          case "probe-rs-show-message": {
            const text = message.body?.message ?? "";
            const severity = message.body?.severity ?? "information";
            sendToClient({
              seq: 0,
              type: "event",
              event: "output",
              body: {
                category:
                  severity === "error" || severity === "warning"
                    ? "stderr"
                    : "console",
                output: `${text}\n`,
              },
            });
            return;
          }
        }
      }

      sendToClient(message);
    },
    () => close(),
  );

  client.on("close", close);
  client.on("error", close);
  child.on("error", (error) => {
    console.error(`dap_proxy: failed to spawn ${upstream}: ${error.message}`);
    close();
  });
  child.on("exit", close);
});

server.on("error", (error) => {
  console.error(`dap_proxy: ${error.message}`);
  process.exit(1);
});

server.listen(port, "127.0.0.1", () => {
  console.log(`dap_proxy: listening on 127.0.0.1:${port}`);
});
