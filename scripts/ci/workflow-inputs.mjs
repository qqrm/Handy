import { appendFileSync, readFileSync } from "node:fs";

// Write only single-line values to the runner's output protocol. Tauri still
// validates the complete release version and asset naming contract at build time.
function singleLine(value, name) {
  if (typeof value !== "string" || /[\r\n\0]/.test(value)) {
    throw new Error(`${name} must be a single-line string`);
  }
  return value;
}

let output;
switch (process.argv[2]) {
  case "version": {
    const config = JSON.parse(
      readFileSync("src-tauri/tauri.conf.json", "utf8"),
    );
    const version = singleLine(config.version, "Application version");
    if (
      !/^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(
        version,
      )
    ) {
      throw new Error(
        "Application version must use semantic version characters",
      );
    }
    output = `version=${version}\n`;
    break;
  }
  case "asset-name": {
    const platform = singleLine(process.env.HANDY_PLATFORM, "Platform");
    const pattern = singleLine(
      process.env.HANDY_ASSET_PATTERN,
      "Asset pattern",
    );
    const prefix = singleLine(
      process.env.HANDY_ASSET_PREFIX ?? "",
      "Asset prefix",
    );
    const name = pattern.replace("[platform]", platform.replace("-latest", ""));
    output = `platform=${prefix ? `${prefix}_` : ""}${name}\n`;
    break;
  }
  default:
    throw new Error("Expected version or asset-name command");
}

if (!process.env.GITHUB_OUTPUT) {
  throw new Error("GitHub runner output file is required");
}
appendFileSync(process.env.GITHUB_OUTPUT, output);
