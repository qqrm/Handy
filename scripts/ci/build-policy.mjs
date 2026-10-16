import { appendFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export function planBuild({ artifactOnly, signBinaries, releaseId }) {
  if (releaseId && (artifactOnly || !signBinaries)) {
    throw new Error(
      "Release assets require signing; artifact-only releases are forbidden",
    );
  }
  if (artifactOnly && signBinaries) {
    throw new Error("Artifact-only mode requires unsigned CI artifacts");
  }
  if (!signBinaries && !artifactOnly) {
    throw new Error("Unsigned builds require explicit artifact-only mode");
  }
  return artifactOnly
    ? {
        label: "unsigned-test-",
        overlay: {
          bundle: {
            createUpdaterArtifacts: false,
            windows: { signCommand: null },
          },
        },
      }
    : { label: "", overlay: null };
}

function booleanInput(env, name) {
  if (env[name] !== "true" && env[name] !== "false") {
    throw new Error(`Missing or invalid boolean input: ${name}`);
  }
  return env[name] === "true";
}

export function prepareBuild(env) {
  const plan = planBuild({
    artifactOnly: booleanInput(env, "HANDY_ARTIFACT_ONLY"),
    signBinaries: booleanInput(env, "HANDY_SIGN_BINARIES"),
    releaseId: env.HANDY_RELEASE_ID ?? "",
  });
  if (!env.GITHUB_OUTPUT || !env.RUNNER_TEMP) {
    throw new Error(
      "GitHub runner output and temporary directories are required",
    );
  }
  let configArgs = "";
  if (plan.overlay) {
    const path = join(env.RUNNER_TEMP, "handy-artifact-only.json");
    writeFileSync(path, JSON.stringify(plan.overlay));
    // The CLI merges this last, after the tracked platform configuration.
    configArgs = `--config "${path.replaceAll("\\", "/")}"`;
  }
  appendFileSync(
    env.GITHUB_OUTPUT,
    `config-args=${configArgs}\nartifact-label=${plan.label}\n`,
  );
  if (plan.overlay && env.GITHUB_STEP_SUMMARY) {
    appendFileSync(
      env.GITHUB_STEP_SUMMARY,
      "Unsigned test artifacts only. No release or updater assets are published. " +
        "These packages are for isolated testing and are not trusted signed releases.\n",
    );
  }
  return plan;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  prepareBuild(process.env);
}
