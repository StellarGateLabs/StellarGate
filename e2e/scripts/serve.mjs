#!/usr/bin/env node
/* Launch the gateway for the end-to-end suite.
 *
 * Playwright's `webServer` wants a command and a URL to poll. The server needs
 * several environment variables and a freshly built binary, and a failing build
 * should print cargo's own diagnostics rather than a bare exit code — so the
 * orchestration lives here instead of in a shell one-liner in the workflow.
 *
 * Dev-only. Nothing in this directory is compiled into the gateway binary.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, "..", "..");
const binary = join(repoRoot, "target", "debug", "stellargate");

if (!existsSync(binary)) {
  console.error(
    `The gateway binary is missing at ${binary}.\n` +
      `Build it first:  cargo build --locked`,
  );
  process.exit(1);
}

const child = spawn(binary, [], {
  cwd: repoRoot,
  stdio: "inherit",
  env: process.env,
});

/* Forward the signals Playwright sends on teardown, so the child gets a chance
   to drain rather than being killed mid-write and leaving a locked SQLite
   file behind for the next run. */
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
