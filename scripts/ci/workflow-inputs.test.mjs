import assert from "node:assert/strict";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const cli = fileURLToPath(new URL("./workflow-inputs.mjs", import.meta.url));

function run(command, env, version) {
  const directory = mkdtempSync(join(tmpdir(), "handy-workflow-inputs-"));
  try {
    mkdirSync(join(directory, "src-tauri"));
    writeFileSync(
      join(directory, "src-tauri/tauri.conf.json"),
      JSON.stringify({ version }),
    );
    const output = join(directory, "outputs");
    const result = spawnSync(process.execPath, [cli, command], {
      cwd: directory,
      env: { ...process.env, ...env, GITHUB_OUTPUT: output },
      encoding: "utf8",
    });
    assert.ifError(result.error);
    return {
      status: result.status,
      stderr: result.stderr,
      output: existsSync(output) ? readFileSync(output, "utf8") : null,
      injected: existsSync(join(directory, "injected")),
    };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test("release versions preserve prerelease and build metadata", () => {
  for (const version of ["0.9.7", "1.2.3-rc.1+build.5"]) {
    const result = run("version", {}, version);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.output, `version=${version}\n`);
  }
});

test("malformed versions cannot inject runner outputs or script source", () => {
  for (const version of [
    undefined,
    123,
    "",
    "1.2.3\nrelease-id=42",
    "1.2.3\rrelease-id=42",
    "1.2.3$(touch injected)",
    '1.2.3"; touch injected; #',
  ]) {
    const result = run("version", {}, version);
    assert.notEqual(result.status, 0);
    assert.equal(result.output, null);
    assert.equal(result.injected, false);
  }
});

test("asset naming preserves the existing prefix and platform substitution", () => {
  for (const prefix of ["", "handy"]) {
    const result = run("asset-name", {
      HANDY_PLATFORM: "macos-latest",
      HANDY_ASSET_PATTERN: "[name]-[platform]-[version]",
      HANDY_ASSET_PREFIX: prefix,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(
      result.output,
      `platform=${prefix ? "handy_" : ""}[name]-macos-[version]\n`,
    );
  }
});

test("asset inputs remain literal data even when they contain shell syntax", () => {
  const prefix = 'test"; touch injected; #';
  const pattern = "[platform]-$(touch injected)-`touch injected`";
  const result = run("asset-name", {
    HANDY_PLATFORM: "windows-latest",
    HANDY_ASSET_PATTERN: pattern,
    HANDY_ASSET_PREFIX: prefix,
  });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(
    result.output,
    `platform=${prefix}_windows-$(touch injected)-\`touch injected\`\n`,
  );
  assert.equal(result.injected, false);
});

test("no asset input can inject an additional runner output", () => {
  for (const name of [
    "HANDY_PLATFORM",
    "HANDY_ASSET_PATTERN",
    "HANDY_ASSET_PREFIX",
  ]) {
    for (const suffix of ["\nrelease-id=42", "\rrelease-id=42"]) {
      const result = run("asset-name", {
        HANDY_PLATFORM: "macos-latest",
        HANDY_ASSET_PATTERN: "[platform]",
        HANDY_ASSET_PREFIX: "handy",
        [name]: `bad${suffix}`,
      });
      assert.notEqual(result.status, 0);
      assert.equal(result.output, null);
    }
  }
});
