#!/usr/bin/env node

/**
 * postinstall.js — downloads the pre-compiled memory-mcp binary
 * from GitHub Releases for the current platform.
 *
 * Zero external dependencies — uses only Node.js built-ins.
 */

const https = require("https");
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");
const os = require("os");
const crypto = require("crypto");

const REPO = "pomazanbohdan/memory-mcp-1file";
const BINARY_NAME = "memory-mcp";

// Map Node.js platform/arch to Rust target triples
const PLATFORM_MAP = {
    "linux-x64": "x86_64-unknown-linux-musl",
    "darwin-x64": "x86_64-apple-darwin",
    "darwin-arm64": "aarch64-apple-darwin",
    "win32-x64": "x86_64-pc-windows-msvc",
};

function getPlatformKey() {
    return `${process.platform}-${process.arch}`;
}

function getTarget() {
    const key = getPlatformKey();
    const target = PLATFORM_MAP[key];
    if (!target) {
        console.error(
            `Unsupported platform: ${key}\n` +
            `Supported platforms: ${Object.keys(PLATFORM_MAP).join(", ")}`
        );
        process.exit(1);
    }
    return target;
}

function getVersion() {
    const pkg = JSON.parse(
        fs.readFileSync(path.join(__dirname, "package.json"), "utf8")
    );
    return pkg.version;
}

function getAssetName(version, target) {
    const ext = target.includes("windows") ? ".zip" : ".tar.gz";
    return `${BINARY_NAME}-${version}-${target}${ext}`;
}

function getDownloadUrl(version, target) {
    return `https://github.com/${REPO}/releases/download/v${version}/${getAssetName(version, target)}`;
}

function getChecksumUrl(version, target) {
    return `${getDownloadUrl(version, target)}.sha256`;
}

/**
 * Follow redirects (GitHub releases use 302 → S3).
 */
function download(url, redirectCount = 0) {
    return new Promise((resolve, reject) => {
        if (redirectCount > 5) {
            return reject(new Error(`Too many redirects for ${url}`));
        }
        if (!url.startsWith("https://")) {
            return reject(new Error(`Refusing non-HTTPS download URL: ${url}`));
        }
        const client = https;
        client
            .get(url, { headers: { "User-Agent": "memory-mcp-npm" } }, (res) => {
                if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
                    const redirect = new URL(res.headers.location, url).toString();
                    return download(redirect, redirectCount + 1).then(resolve, reject);
                }
                if (res.statusCode !== 200) {
                    return reject(
                        new Error(`Download failed: HTTP ${res.statusCode} for ${url}`)
                    );
                }
                const chunks = [];
                res.on("data", (chunk) => chunks.push(chunk));
                res.on("end", () => resolve(Buffer.concat(chunks)));
                res.on("error", reject);
            })
            .on("error", reject);
    });
}


function sha256Hex(buffer) {
    return crypto.createHash("sha256").update(buffer).digest("hex");
}

function parseChecksumFile(text) {
    const token = text.trim().split(/\s+/)[0];
    if (!/^[a-fA-F0-9]{64}$/.test(token)) {
        throw new Error("Invalid SHA256 checksum format");
    }
    return token.toLowerCase();
}

/**
 * Extract .tar.gz using Node.js built-in zlib + tar command.
 */
async function extractTarGz(buffer, destDir) {
    fs.mkdirSync(destDir, { recursive: true });
    const tmpFile = path.join(os.tmpdir(), `memory-mcp-${Date.now()}.tar.gz`);
    fs.writeFileSync(tmpFile, buffer);
    try {
        execSync(`tar -xzf "${tmpFile}" -C "${destDir}"`, { stdio: "pipe" });
    } finally {
        fs.unlinkSync(tmpFile);
    }
}

/**
 * Extract .zip using unzip command (available on Windows via PowerShell).
 */
async function extractZip(buffer, destDir) {
    fs.mkdirSync(destDir, { recursive: true });
    const tmpFile = path.join(os.tmpdir(), `memory-mcp-${Date.now()}.zip`);
    fs.writeFileSync(tmpFile, buffer);
    try {
        if (process.platform === "win32") {
            execSync(
                `powershell -Command "Expand-Archive -Path '${tmpFile}' -DestinationPath '${destDir}' -Force"`,
                { stdio: "pipe" }
            );
        } else {
            execSync(`unzip -o "${tmpFile}" -d "${destDir}"`, { stdio: "pipe" });
        }
    } finally {
        fs.unlinkSync(tmpFile);
    }
}

async function main() {
    const target = getTarget();
    const version = getVersion();
    const url = getDownloadUrl(version, target);
    const binDir = path.join(__dirname, "bin");
    const isWindows = target.includes("windows");
    const binaryPath = path.join(
        binDir,
        isWindows ? `${BINARY_NAME}.exe` : BINARY_NAME
    );

    // Skip if binary already exists
    if (fs.existsSync(binaryPath)) {
        console.log(`memory-mcp binary already exists at ${binaryPath}`);
        return;
    }

    console.log(`Downloading memory-mcp v${version} for ${target}...`);
    console.log(`  URL: ${url}`);

    try {
        const [buffer, checksumBuffer] = await Promise.all([
            download(url),
            download(getChecksumUrl(version, target)),
        ]);

        const expected = parseChecksumFile(checksumBuffer.toString("utf8"));
        const actual = sha256Hex(buffer);
        if (actual !== expected) {
            throw new Error(`Checksum mismatch for downloaded binary (expected ${expected}, got ${actual})`);
        }

        if (isWindows) {
            await extractZip(buffer, binDir);
        } else {
            await extractTarGz(buffer, binDir);
        }

        // Make binary executable on Unix
        if (!isWindows) {
            fs.chmodSync(binaryPath, 0o755);
        }

        console.log(`✅ memory-mcp installed successfully at ${binaryPath}`);
    } catch (err) {
        console.error(`\n❌ Failed to install memory-mcp binary:`);
        console.error(`   ${err.message}`);
        console.error(
            `\nYou can manually download the binary from:`
        );
        console.error(
            `   https://github.com/${REPO}/releases/tag/v${version}`
        );
        process.exit(1);
    }
}

main();
