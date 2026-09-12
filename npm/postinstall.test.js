const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const {
    extractZip,
    findBinary,
    installExtractedBinary,
    validateZipArchive,
} = require("./postinstall");

function withTempDirectory(callback) {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "memory-mcp-postinstall-test-"));
    try {
        return callback(directory);
    } finally {
        fs.rmSync(directory, { recursive: true, force: true });
    }
}

function crc32(buffer) {
    let crc = 0xffffffff;
    for (const byte of buffer) {
        crc ^= byte;
        for (let bit = 0; bit < 8; bit++) {
            crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
        }
    }
    return (crc ^ 0xffffffff) >>> 0;
}

function makeZip(entries) {
    const localFiles = [];
    const centralDirectory = [];
    let localOffset = 0;

    for (const entry of entries) {
        const name = Buffer.from(entry.name, "utf8");
        const data = Buffer.from(entry.content ?? "");
        const checksum = crc32(data);
        const local = Buffer.alloc(30 + name.length + data.length);
        local.writeUInt32LE(0x04034b50, 0);
        local.writeUInt16LE(20, 4);
        local.writeUInt16LE(0x800, 6);
        local.writeUInt32LE(checksum, 14);
        local.writeUInt32LE(data.length, 18);
        local.writeUInt32LE(data.length, 22);
        local.writeUInt16LE(name.length, 26);
        name.copy(local, 30);
        data.copy(local, 30 + name.length);
        localFiles.push(local);

        const central = Buffer.alloc(46 + name.length);
        central.writeUInt32LE(0x02014b50, 0);
        central.writeUInt16LE(entry.versionMadeBy ?? 20, 4);
        central.writeUInt16LE(20, 6);
        central.writeUInt16LE(0x800, 8);
        central.writeUInt32LE(checksum, 16);
        central.writeUInt32LE(data.length, 20);
        central.writeUInt32LE(data.length, 24);
        central.writeUInt16LE(name.length, 28);
        central.writeUInt32LE(entry.externalAttributes ?? 0, 38);
        central.writeUInt32LE(localOffset, 42);
        name.copy(central, 46);
        centralDirectory.push(central);
        localOffset += local.length;
    }

    const centralDirectorySize = centralDirectory.reduce(
        (size, entry) => size + entry.length,
        0
    );
    const end = Buffer.alloc(22);
    end.writeUInt32LE(0x06054b50, 0);
    end.writeUInt16LE(entries.length, 8);
    end.writeUInt16LE(entries.length, 10);
    end.writeUInt32LE(centralDirectorySize, 12);
    end.writeUInt32LE(localOffset, 16);
    return Buffer.concat([...localFiles, ...centralDirectory, end]);
}

test("validates a root or nested ZIP binary before extraction", () => {
    assert.doesNotThrow(() =>
        validateZipArchive(
            makeZip([
                {
                    name: "target/x86_64-pc-windows-msvc/release/memory-mcp.exe",
                    content: "binary",
                },
            ]),
            "memory-mcp.exe"
        )
    );
});
test("extracts a nested ZIP binary into the extraction directory", async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "memory-mcp-zip-test-"));
    try {
        const extractedDirectory = path.join(directory, "extracted");
        await extractZip(
            makeZip([
                {
                    name: "target/x86_64-pc-windows-msvc/release/memory-mcp.exe",
                    content: "zip-binary",
                },
            ]),
            extractedDirectory,
            "memory-mcp.exe"
        );
        assert.equal(
            fs.readFileSync(
                path.join(
                    extractedDirectory,
                    "target",
                    "x86_64-pc-windows-msvc",
                    "release",
                    "memory-mcp.exe"
                ),
                "utf8"
            ),
            "zip-binary"
        );
    } finally {
        fs.rmSync(directory, { recursive: true, force: true });
    }
});

test("rejects duplicate ZIP binary entries before extraction", () => {
    assert.throws(
        () =>
            validateZipArchive(
                makeZip([
                    { name: "one/memory-mcp.exe", content: "one" },
                    { name: "two/MEMORY-MCP.EXE", content: "two" },
                ]),
                "memory-mcp.exe"
            ),
        /Expected exactly one memory-mcp\.exe binary in ZIP archive/
    );
});

test("rejects unsafe ZIP paths and symlink metadata before extraction", () => {
    assert.throws(
        () =>
            validateZipArchive(
                makeZip([{ name: "../memory-mcp.exe", content: "binary" }]),
                "memory-mcp.exe"
            ),
        /Refusing unsafe ZIP entry path/
    );
    for (const name of ["memory-mcp.exe.", "memory-mcp.exe::$DATA"]) {
        assert.throws(
            () => validateZipArchive(makeZip([{ name, content: "binary" }]), "memory-mcp.exe"),
            /Refusing unsafe ZIP entry path/
        );
    }
    assert.throws(
        () =>
            validateZipArchive(
                makeZip([
                    {
                        name: "memory-mcp.exe",
                        content: "binary",
                        versionMadeBy: 0x0314,
                        externalAttributes: 0xa0000000,
                    },
                ]),
                "memory-mcp.exe"
            ),
        /Refusing symbolic link in ZIP archive/
    );
});

test("normalizes a single nested Windows binary", () => {
    withTempDirectory((directory) => {
        const sourcePath = path.join(
            directory,
            "target",
            "x86_64-pc-windows-msvc",
            "release",
            "memory-mcp.exe"
        );
        const destinationPath = path.join(directory, "package", "bin", "memory-mcp.exe");
        fs.mkdirSync(path.dirname(sourcePath), { recursive: true });
        fs.writeFileSync(sourcePath, "windows-binary");

        installExtractedBinary(directory, destinationPath, "memory-mcp.exe");

        assert.equal(fs.readFileSync(destinationPath, "utf8"), "windows-binary");
        assert.equal(fs.statSync(destinationPath).isFile(), true);
    });
});

test("rejects archives containing duplicate matching binaries", () => {
    withTempDirectory((directory) => {
        const firstPath = path.join(directory, "one", "memory-mcp.exe");
        const secondPath = path.join(directory, "two", "memory-mcp.exe");
        fs.mkdirSync(path.dirname(firstPath), { recursive: true });
        fs.mkdirSync(path.dirname(secondPath), { recursive: true });
        fs.writeFileSync(firstPath, "first");
        fs.writeFileSync(secondPath, "second");

        assert.throws(
            () => findBinary(directory, "memory-mcp.exe"),
            /Expected exactly one memory-mcp\.exe binary/
        );
    });
});

test("rejects symbolic links in an extracted archive", (t) => {
    withTempDirectory((directory) => {
        const sourcePath = path.join(directory, "real-memory-mcp.exe");
        const linkPath = path.join(directory, "memory-mcp.exe");
        fs.writeFileSync(sourcePath, "binary");
        try {
            fs.symlinkSync(sourcePath, linkPath, "file");
        } catch (err) {
            if (!["EACCES", "EPERM", "ENOSYS"].includes(err.code)) {
                throw err;
            }
            const targetDirectory = path.join(directory, "real-memory-mcp");
            fs.mkdirSync(targetDirectory);
            try {
                fs.symlinkSync(targetDirectory, linkPath, "junction");
            } catch (junctionErr) {
                if (["EACCES", "EPERM", "ENOSYS"].includes(junctionErr.code)) {
                    t.skip(`symbolic links are unavailable: ${junctionErr.code}`);
                    return;
                }
                throw junctionErr;
            }
        }

        assert.throws(
            () => findBinary(directory, "memory-mcp.exe"),
            /Refusing symbolic link/
        );
    });
});

test("does not overwrite an existing destination", () => {
    withTempDirectory((directory) => {
        const archiveDirectory = path.join(directory, "archive");
        const sourcePath = path.join(archiveDirectory, "memory-mcp.exe");
        const destinationPath = path.join(directory, "installed", "memory-mcp.exe");
        fs.mkdirSync(archiveDirectory, { recursive: true });
        fs.writeFileSync(sourcePath, "new-binary");
        fs.mkdirSync(path.dirname(destinationPath), { recursive: true });
        fs.writeFileSync(destinationPath, "existing-binary");

        assert.throws(
            () => installExtractedBinary(archiveDirectory, destinationPath, "memory-mcp.exe"),
            /Refusing to overwrite existing binary/
        );
        assert.equal(fs.readFileSync(destinationPath, "utf8"), "existing-binary");
    });
});
