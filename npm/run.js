#!/usr/bin/env node

/**
 * run.js — entry point for `npx memory-mcp` / `bunx memory-mcp`.
 * Finds and executes the pre-compiled binary, proxying stdio and args.
 */

const { spawn } = require("child_process");
const path = require("path");
const fs = require("fs");

const isWindows = process.platform === "win32";
const binaryName = isWindows ? "memory-mcp.exe" : "memory-mcp";
const binaryPath = path.join(__dirname, "bin", binaryName);

if (!fs.existsSync(binaryPath)) {
    console.error(
        `❌ memory-mcp binary not found at ${binaryPath}\n` +
        `   Run "node postinstall.js" to download it, or reinstall the package.`
    );
    process.exit(1);
}

// npm/npx may forward its option separator to the package command.
const forwardedArgs =
    process.argv[2] === "--" ? process.argv.slice(3) : process.argv.slice(2);

const child = spawn(binaryPath, forwardedArgs, {
    stdio: "inherit",
    env: process.env,
});

child.on("error", (err) => {
    console.error(`Failed to start memory-mcp: ${err.message}`);
    process.exit(1);
});

child.on("exit", (code, signal) => {
    if (signal) {
        process.kill(process.pid, signal);
    } else {
        process.exit(code ?? 1);
    }
});
