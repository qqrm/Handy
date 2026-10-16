import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { planBuild, prepareBuild } from "./build-policy.mjs";

test("release combinations reject unsigned or artifact-only publication", () => {
  for (const artifactOnly of [false, true]) {
    for (const signBinaries of [false, true]) {
      for (const uploadArtifacts of [false, true]) {
        const inputs = {
          artifactOnly,
          signBinaries,
          uploadArtifacts,
          releaseId: "123",
        };
        if (signBinaries && !artifactOnly) {
          assert.deepEqual(planBuild(inputs), { label: "", overlay: null });
        } else {
          assert.throws(
            () => planBuild(inputs),
            /Release assets require signing/,
          );
        }
      }
    }
  }
});

test("unsigned builds require explicit artifact intent", () => {
  for (const inputs of [
    { artifactOnly: false, signBinaries: false, uploadArtifacts: true },
    { artifactOnly: true, signBinaries: true, uploadArtifacts: true },
  ]) {
    assert.throws(() => planBuild({ ...inputs, releaseId: "" }));
  }
});

test("artifact-only checks can avoid retained installer storage", () => {
  for (const uploadArtifacts of [false, true]) {
    assert.equal(
      planBuild({
        artifactOnly: true,
        signBinaries: false,
        releaseId: "",
        uploadArtifacts,
      }).label,
      "unsigned-test-",
    );
  }
});

test("artifact overlay preserves production identity and updater trust", () => {
  const configPath = new URL(
    "../../src-tauri/tauri.conf.json",
    import.meta.url,
  );
  const before = readFileSync(configPath);
  const directory = mkdtempSync(join(tmpdir(), "handy-build-policy-"));
  try {
    const env = {
      HANDY_ARTIFACT_ONLY: "true",
      HANDY_SIGN_BINARIES: "false",
      HANDY_UPLOAD_ARTIFACTS: "true",
      HANDY_RELEASE_ID: "",
      RUNNER_TEMP: directory,
      GITHUB_OUTPUT: join(directory, "outputs"),
    };
    prepareBuild(env);
    const overlay = JSON.parse(
      readFileSync(join(directory, "handy-artifact-only.json")),
    );
    assert.deepEqual(overlay, {
      bundle: {
        createUpdaterArtifacts: false,
        windows: { signCommand: null },
      },
    });
    assert.deepEqual(readFileSync(configPath), before);
    const production = JSON.parse(before);
    assert.equal(production.bundle.createUpdaterArtifacts, true);
    assert.match(production.bundle.windows.signCommand, /trusted-signing-cli/);
    assert.ok(production.plugins.updater.pubkey);
    assert.deepEqual(production.plugins.updater.endpoints, [
      "https://github.com/cjpais/Handy/releases/latest/download/latest.json",
    ]);
    assert.match(
      readFileSync(env.GITHUB_OUTPUT, "utf8"),
      /artifact-label=unsigned-test-/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("rejected release cannot write an overlay or outputs", () => {
  const directory = mkdtempSync(join(tmpdir(), "handy-build-policy-"));
  try {
    assert.throws(
      () =>
        prepareBuild({
          HANDY_ARTIFACT_ONLY: "true",
          HANDY_SIGN_BINARIES: "false",
          HANDY_UPLOAD_ARTIFACTS: "true",
          HANDY_RELEASE_ID: "123",
          RUNNER_TEMP: directory,
          GITHUB_OUTPUT: join(directory, "outputs"),
        }),
      /Release assets require signing/,
    );
    assert.deepEqual(readdirSync(directory), []);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("signed release creates no artifact overlay or unsigned label", () => {
  const directory = mkdtempSync(join(tmpdir(), "handy-build-policy-"));
  try {
    const output = join(directory, "outputs");
    prepareBuild({
      HANDY_ARTIFACT_ONLY: "false",
      HANDY_SIGN_BINARIES: "true",
      HANDY_UPLOAD_ARTIFACTS: "false",
      HANDY_RELEASE_ID: "123",
      RUNNER_TEMP: directory,
      GITHUB_OUTPUT: output,
    });
    assert.deepEqual(readdirSync(directory), ["outputs"]);
    assert.equal(
      readFileSync(output, "utf8"),
      "config-args=\nartifact-label=\n",
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("ambiguous boolean inputs fail closed", () => {
  for (const name of ["HANDY_ARTIFACT_ONLY", "HANDY_SIGN_BINARIES"]) {
    const env = {
      HANDY_ARTIFACT_ONLY: "true",
      HANDY_SIGN_BINARIES: "false",
      HANDY_UPLOAD_ARTIFACTS: "true",
    };
    for (const value of [undefined, "", "0", "False"]) {
      assert.throws(
        () => prepareBuild({ ...env, [name]: value }),
        /Missing or invalid boolean input/,
      );
    }
  }
});
