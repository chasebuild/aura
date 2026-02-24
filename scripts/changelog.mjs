#!/usr/bin/env node

import { spawnSync } from "node:child_process";

const RELEASE_PLZ_BIN = process.env.RELEASE_PLZ_BIN || "release-plz";
const COMMANDS = new Set(["update", "release-pr", "release"]);

function printUsage() {
  console.log(`Usage:
  npm run changelog -- [release-plz-args]
  npm run release:pr -- [release-plz-args]
  npm run release:publish -- [release-plz-args]

Examples:
  npm run changelog -- --dry-run
  npm run release:pr -- --dry-run
  npm run release:publish -- --dry-run

Notes:
  - This script is a thin wrapper around release-plz.
  - Install release-plz locally first:
      cargo install release-plz
  - Override binary path:
      RELEASE_PLZ_BIN=/custom/path/release-plz npm run changelog
`);
}

function fail(message) {
  console.error(`error: ${message}`);
  process.exit(1);
}

function resolveCommand(argv) {
  const args = [...argv];
  if (args.length === 0) {
    return { command: "update", passthrough: [] };
  }

  const first = args[0];
  if (first === "-h" || first === "--help") {
    printUsage();
    process.exit(0);
  }

  if (COMMANDS.has(first)) {
    return { command: first, passthrough: args.slice(1) };
  }

  return { command: "update", passthrough: args };
}

function runReleasePlz(command, passthrough) {
  const args = [command, ...passthrough];
  const result = spawnSync(RELEASE_PLZ_BIN, args, { stdio: "inherit" });

  if (result.error) {
    if (result.error.code === "ENOENT") {
      fail(
        `could not find '${RELEASE_PLZ_BIN}'. Install with 'cargo install release-plz' or set RELEASE_PLZ_BIN.`
      );
    }
    fail(result.error.message);
  }

  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

const { command, passthrough } = resolveCommand(process.argv.slice(2));
runReleasePlz(command, passthrough);
