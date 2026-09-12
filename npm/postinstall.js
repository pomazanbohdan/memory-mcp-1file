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
const { execFileSync } = require("child_process");
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
        https
            .get(url, { headers: { "User-Agent": "memory-mcp-npm" } }, (res) => {
                if (
                    res.statusCode >= 300 &&
                    res.statusCode < 400 &&
                    res.headers.location
                ) {
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

function removeTemporaryDirectory(directory) {
    try {
        fs.rmSync(directory, { recursive: true, force: true });
    } catch (err) {
        console.warn(`Warning: unable to remove temporary directory ${directory}: ${err.message}`);
    }
}

/**
 * Extract .tar.gz using Node.js built-in process spawning.
 */
async function extractTarGz(buffer, destDir) {
    fs.mkdirSync(destDir, { recursive: true });
    const archiveDir = fs.mkdtempSync(path.join(os.tmpdir(), "memory-mcp-archive-"));
    const archivePath = path.join(archiveDir, "archive.tar.gz");
    fs.writeFileSync(archivePath, buffer);
    try {
        execFileSync("tar", ["-xzf", archivePath, "-C", destDir], {
            stdio: "pipe",
        });
    } finally {
        removeTemporaryDirectory(archiveDir);
    }
}

/**
 * Validate ZIP metadata before extraction. This prevents extractors that
 * overwrite duplicate entries from hiding multiple binaries or symlinks.
 */
function validateWindowsEntryPath(entryName, externalAttributes) {
    const normalizedName = entryName.replace(/\\/g, "/");
    const isDirectory =
        normalizedName.endsWith("/") || (externalAttributes & 0x10) !== 0;
    const rawParts = normalizedName.split("/");
    const pathParts = isDirectory ? rawParts.slice(0, -1) : rawParts;
    const deviceName = /^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/;

    if (
        !entryName ||
        normalizedName.startsWith("/") ||
        /^[A-Za-z]:/.test(normalizedName) ||
        pathParts.length === 0 ||
        pathParts.includes("..")
    ) {
        throw new Error(`Refusing unsafe ZIP entry path: ${entryName}`);
    }

    for (const part of pathParts) {
        if (
            !part ||
            part === "." ||
            /[<>:"|?*\u0000-\u001f]/.test(part) ||
            /[. ]$/.test(part) ||
            deviceName.test(part.split(".")[0].toUpperCase())
        ) {
            throw new Error(`Refusing unsafe ZIP entry path: ${entryName}`);
        }
    }

    return {
        isDirectory,
        basename: pathParts[pathParts.length - 1].replace(/[. ]+$/, "").toLowerCase(),
    };
}

/**
 * Validate ZIP metadata before extraction. This prevents extractors that
 * overwrite duplicate entries from hiding multiple binaries or symlinks.
 */
function validateZipArchive(buffer, binaryFileName) {
    const EOCD_SIGNATURE = 0x06054b50;
    const CENTRAL_DIRECTORY_SIGNATURE = 0x02014b50;
    const eocdSearchStart = Math.max(0, buffer.length - 0xffff - 22);
    let eocdOffset = -1;

    for (let offset = buffer.length - 22; offset >= eocdSearchStart; offset--) {
        if (buffer.readUInt32LE(offset) === EOCD_SIGNATURE) {
            eocdOffset = offset;
            break;
        }
    }

    if (eocdOffset < 0 || eocdOffset + 22 > buffer.length) {
        throw new Error("Invalid ZIP archive: end-of-central-directory record not found");
    }

    const commentLength = buffer.readUInt16LE(eocdOffset + 20);
    if (eocdOffset + 22 + commentLength > buffer.length) {
        throw new Error("Invalid ZIP archive: truncated end-of-central-directory record");
    }

    const entryCount = buffer.readUInt16LE(eocdOffset + 10);
    const centralDirectorySize = buffer.readUInt32LE(eocdOffset + 12);
    const centralDirectoryOffset = buffer.readUInt32LE(eocdOffset + 16);
    if (
        entryCount === 0xffff ||
        centralDirectorySize === 0xffffffff ||
        centralDirectoryOffset === 0xffffffff
    ) {
        throw new Error("ZIP64 release archives are not supported");
    }
    if (
        centralDirectoryOffset + centralDirectorySize > eocdOffset ||
        centralDirectoryOffset + centralDirectorySize > buffer.length
    ) {
        throw new Error("Invalid ZIP archive: central directory is outside the archive");
    }

    const expectedName = binaryFileName
        .replace(/[. ]+$/, "")
        .toLowerCase();
    const centralDirectoryEnd = centralDirectoryOffset + centralDirectorySize;
    let offset = centralDirectoryOffset;
    let matchingBinaryCount = 0;

    for (let index = 0; index < entryCount; index++) {
        if (
            offset + 46 > centralDirectoryEnd ||
            buffer.readUInt32LE(offset) !== CENTRAL_DIRECTORY_SIGNATURE
        ) {
            throw new Error("Invalid ZIP archive: malformed central directory entry");
        }

        const flags = buffer.readUInt16LE(offset + 8);
        const nameLength = buffer.readUInt16LE(offset + 28);
        const extraLength = buffer.readUInt16LE(offset + 30);
        const entryCommentLength = buffer.readUInt16LE(offset + 32);
        const externalAttributes = buffer.readUInt32LE(offset + 38);
        const entryEnd =
            offset + 46 + nameLength + extraLength + entryCommentLength;
        if (entryEnd > centralDirectoryEnd) {
            throw new Error("Invalid ZIP archive: truncated central directory entry");
        }

        const nameBytes = buffer.subarray(offset + 46, offset + 46 + nameLength);
        if (nameBytes.includes(0)) {
            throw new Error("Invalid ZIP archive: entry name contains NUL");
        }
        const entryName = nameBytes.toString(flags & 0x800 ? "utf8" : "latin1");
        const { isDirectory, basename } = validateWindowsEntryPath(
            entryName,
            externalAttributes
        );

        const unixFileType = (externalAttributes >>> 16) & 0xf000;
        if (unixFileType === 0xa000) {
            throw new Error(`Refusing symbolic link in ZIP archive: ${entryName}`);
        }

        if (basename === expectedName) {
            if (isDirectory) {
                throw new Error(`Refusing directory named as binary: ${entryName}`);
            }
            matchingBinaryCount++;
        }

        offset = entryEnd;
    }

    if (offset !== centralDirectoryEnd) {
        throw new Error("Invalid ZIP archive: central directory size mismatch");
    }
    if (matchingBinaryCount !== 1) {
        throw new Error(
            `Expected exactly one ${binaryFileName} binary in ZIP archive, found ${matchingBinaryCount}`
        );
    }
}

function ensureDirectoryPath(directory) {
    const absoluteDirectory = path.resolve(directory);
    const root = path.parse(absoluteDirectory).root;
    const relativeSegments = path
        .relative(root, absoluteDirectory)
        .split(path.sep)
        .filter(Boolean);
    let current = root;

    for (const segment of relativeSegments) {
        current = path.join(current, segment);
        let stats;
        try {
            stats = fs.lstatSync(current);
        } catch (err) {
            if (err.code !== "ENOENT") {
                throw err;
            }
            fs.mkdirSync(current);
            stats = fs.lstatSync(current);
        }
        if (stats.isSymbolicLink()) {
            throw new Error(`Refusing symbolic link in destination path: ${current}`);
        }
        if (!stats.isDirectory()) {
            throw new Error(`Destination path is not a directory: ${current}`);
        }
    }
}


/**
 * Extract .zip using PowerShell on Windows and unzip elsewhere.
 */
async function extractZip(
    buffer,
    destDir,
    binaryFileName = `${BINARY_NAME}.exe`
) {
    validateZipArchive(buffer, binaryFileName);
    fs.mkdirSync(destDir, { recursive: true });
    const archiveDir = fs.mkdtempSync(path.join(os.tmpdir(), "memory-mcp-archive-"));
    const archivePath = path.join(archiveDir, "archive.zip");
    fs.writeFileSync(archivePath, buffer);
    try {
        if (process.platform === "win32") {
            execFileSync(
                "powershell.exe",
                [
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "& { param($archive, $destination); Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force }",
                    archivePath,
                    destDir,
                ],
                { stdio: "pipe" }
            );
        } else {
            execFileSync("unzip", ["-o", archivePath, "-d", destDir], {
                stdio: "pipe",
            });
        }
    } finally {
        removeTemporaryDirectory(archiveDir);
    }
}

/**
 * Find exactly one regular file with the expected binary name.
 *
 * Archives produced by the release workflow contain the binary at their root,
 * but older assets may contain a target/<triple>/release/ prefix. Scanning the
 * extracted tree keeps the installer compatible with both layouts without
 * trusting symlinks or overwriting arbitrary files.
 */
function findBinary(extractedDir, binaryFileName) {
    const expectedName = binaryFileName.toLowerCase();
    const matches = [];
    const pending = [extractedDir];

    while (pending.length > 0) {
        const currentDir = pending.pop();
        for (const entry of fs.readdirSync(currentDir)) {
            const entryPath = path.join(currentDir, entry);
            const stats = fs.lstatSync(entryPath);

            if (stats.isSymbolicLink()) {
                throw new Error(
                    `Refusing symbolic link in extracted archive: ${path.relative(extractedDir, entryPath)}`
                );
            }
            if (stats.isDirectory()) {
                pending.push(entryPath);
                continue;
            }
            if (stats.isFile() && entry.toLowerCase() === expectedName) {
                matches.push(entryPath);
            }
        }
    }

    if (matches.length === 0) {
        throw new Error(
            `No ${binaryFileName} binary found in extracted release archive`
        );
    }
    if (matches.length !== 1) {
        throw new Error(
            `Expected exactly one ${binaryFileName} binary in extracted release archive, found ${matches.length}`
        );
    }
    return matches[0];
}

/**
 * Copy a validated binary into its final package path without overwriting it.
 */
function installExtractedBinary(extractedDir, destinationPath, binaryFileName) {
    const sourcePath = findBinary(extractedDir, binaryFileName);
    ensureDirectoryPath(path.dirname(destinationPath));
    try {
        fs.copyFileSync(sourcePath, destinationPath, fs.constants.COPYFILE_EXCL);
    } catch (err) {
        if (err.code === "EEXIST") {
            throw new Error(`Refusing to overwrite existing binary at ${destinationPath}`);
        }
        throw err;
    }
    return sourcePath;
}

function hasUsableInstalledBinary(binaryPath) {
    let stats;
    try {
        stats = fs.lstatSync(binaryPath);
    } catch (err) {
        if (err.code === "ENOENT") {
            return false;
        }
        throw err;
    }

    if (stats.isSymbolicLink()) {
        throw new Error(`Refusing to use symbolic link as installed binary: ${binaryPath}`);
    }
    if (!stats.isFile()) {
        throw new Error(`Installed binary path is not a regular file: ${binaryPath}`);
    }
    return true;
}


async function main() {
    const target = getTarget();
    const version = getVersion();
    const url = getDownloadUrl(version, target);
    const binDir = path.join(__dirname, "bin");
    const isWindows = target.includes("windows");
    const binaryFileName = isWindows ? `${BINARY_NAME}.exe` : BINARY_NAME;
    const binaryPath = path.join(binDir, binaryFileName);

    ensureDirectoryPath(binDir);
    // Skip only an existing regular file. Never silently trust a symlink or directory.
    if (hasUsableInstalledBinary(binaryPath)) {
        console.log(`memory-mcp binary already exists at ${binaryPath}`);
        return;
    }

    console.log(`Downloading memory-mcp v${version} for ${target}...`);
    console.log(`  URL: ${url}`);

    let extractionDir;
    try {
        const [buffer, checksumBuffer] = await Promise.all([
            download(url),
            download(getChecksumUrl(version, target)),
        ]);

        const expected = parseChecksumFile(checksumBuffer.toString("utf8"));
        const actual = sha256Hex(buffer);
        if (actual !== expected) {
            throw new Error(
                `Checksum mismatch for downloaded binary (expected ${expected}, got ${actual})`
            );
        }

        extractionDir = fs.mkdtempSync(path.join(os.tmpdir(), "memory-mcp-install-"));
        if (isWindows) {
            await extractZip(buffer, extractionDir, binaryFileName);
        } else {
            await extractTarGz(buffer, extractionDir);
        }

        installExtractedBinary(extractionDir, binaryPath, binaryFileName);

        // Make binary executable on Unix.
        if (!isWindows) {
            fs.chmodSync(binaryPath, 0o755);
        }

        console.log(`✅ memory-mcp installed successfully at ${binaryPath}`);
    } catch (err) {
        console.error(`\n❌ Failed to install memory-mcp binary:`);
        console.error(`   ${err.message}`);
        console.error(`\nYou can manually download the binary from:`);
        console.error(`   https://github.com/${REPO}/releases/tag/v${version}`);
        process.exitCode = 1;
    } finally {
        if (extractionDir) {
            removeTemporaryDirectory(extractionDir);
        }
    }
}

if (require.main === module) {
    main().catch((err) => {
        console.error(`\n❌ Failed to install memory-mcp binary:`);
        console.error(`   ${err.message}`);
        process.exitCode = 1;
    });
}

module.exports = {
    BINARY_NAME,
    PLATFORM_MAP,
    getAssetName,
    getChecksumUrl,
    getDownloadUrl,
    extractTarGz,
    extractZip,
    findBinary,
    validateZipArchive,
    installExtractedBinary,
    parseChecksumFile,
    sha256Hex,
};
