import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { access, chmod, lstat, mkdir, mkdtemp, readFile, readlink, rm, symlink, writeFile } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const helperPath = join(repoRoot, "runner", "install-codex-package.sh");
const codexVersion = "0.151.0";

async function executable(path, contents) {
  await writeFile(path, contents, "utf8");
  await chmod(path, 0o755);
}

async function fixture({ manifestVersion = codexVersion, includeHelper = true } = {}) {
  const root = await mkdtemp(join(tmpdir(), "pillbox-codex-package-"));
  const scratchHome = join(root, "scratch-home");
  const packageRoot = join(
    scratchHome,
    ".codex",
    "packages",
    "standalone",
    "releases",
    `${codexVersion}-x86_64-unknown-linux-gnu`,
  );
  const releaseRoot = dirname(packageRoot);
  const currentPackage = join(releaseRoot, "current");
  const launcherDir = join(scratchHome, ".local", "bin");
  const imageRoot = join(root, "image");
  const binDir = join(imageRoot, "usr", "local", "bin");

  await mkdir(join(packageRoot, "bin"), { recursive: true });
  await mkdir(join(packageRoot, "codex-resources", "zsh", "bin"), { recursive: true });
  await mkdir(join(packageRoot, "codex-path"), { recursive: true });
  await mkdir(launcherDir, { recursive: true });
  await writeFile(
    join(packageRoot, "codex-package.json"),
    JSON.stringify({
      layoutVersion: 1,
      version: manifestVersion,
      target: "x86_64-unknown-linux-gnu",
      variant: "codex",
      entrypoint: "bin/codex",
      resourcesDir: "codex-resources",
      pathDir: "codex-path",
    }),
    "utf8",
  );
  await executable(join(packageRoot, "bin", "codex"), "#!/bin/sh\nexit 0\n");
  if (includeHelper) {
    await executable(join(packageRoot, "bin", "codex-code-mode-host"), "#!/bin/sh\nexit 0\n");
  }
  await executable(join(packageRoot, "codex-resources", "zsh", "bin", "zsh"), "#!/bin/sh\nexit 0\n");
  await executable(join(packageRoot, "codex-path", "rg"), "#!/bin/sh\nexit 0\n");
  await symlink("bin/codex", join(packageRoot, "codex"));
  await symlink(relative(releaseRoot, packageRoot), currentPackage);
  await symlink(relative(launcherDir, join(currentPackage, "bin", "codex")), join(launcherDir, "codex"));

  return { root, scratchHome, packageRoot, imageRoot, binDir };
}

function runHelper(fixtureData) {
  const result = spawnSync(
    "sh",
    [
      helperPath,
      fixtureData.scratchHome,
      codexVersion,
      join(fixtureData.imageRoot, "opt", "codex"),
      fixtureData.binDir,
    ],
    { encoding: "utf8" },
  );
  return {
    ...result,
    output: `${result.stdout ?? ""}${result.stderr ?? ""}`,
  };
}

async function cleanup(fixtureData) {
  await rm(fixtureData.root, { recursive: true, force: true });
}

test("preserves the complete package after installer scratch cleanup", async () => {
  const fixtureData = await fixture();
  try {
    const result = runHelper(fixtureData);
    assert.equal(result.status, 0, result.output);

    await rm(fixtureData.scratchHome, { recursive: true, force: true });
    const stable = join(
      fixtureData.imageRoot,
      "opt",
      "codex",
      "packages",
      "standalone",
      "releases",
      codexVersion,
    );
    const manifest = JSON.parse(await readFile(join(stable, "codex-package.json"), "utf8"));
    assert.equal(manifest.version, codexVersion);
    assert.equal(manifest.entrypoint, "bin/codex");
    assert.equal(await readlink(join(stable, "codex")), "bin/codex");
    assert.equal(await readlink(join(fixtureData.binDir, "codex")), join(stable, "bin", "codex"));
    assert.equal(
      await readlink(join(fixtureData.binDir, "codex-code-mode-host")),
      join(stable, "bin", "codex-code-mode-host"),
    );
    for (const path of [
      join(stable, "bin", "codex"),
      join(stable, "bin", "codex-code-mode-host"),
      join(stable, "codex-resources", "zsh", "bin", "zsh"),
      join(stable, "codex-path", "rg"),
    ]) {
      await access(path, fsConstants.X_OK);
    }
    assert.equal((await lstat(join(fixtureData.binDir, "codex"))).isSymbolicLink(), true);
    await assert.rejects(access(fixtureData.scratchHome));

    const dockerignore = await readFile(join(repoRoot, ".dockerignore"), "utf8");
    assert.match(dockerignore, /^!runner\/install-codex-package\.sh$/m);
    const dockerfile = await readFile(join(repoRoot, "runner", "Dockerfile"), "utf8");
    assert.match(dockerfile, /^ARG CODEX_VERSION=0\.151\.0$/m);
    assert.match(dockerfile, /COPY runner\/install-codex-package\.sh/);
  } finally {
    await cleanup(fixtureData);
  }
});

test("fails loudly when the code-mode host is missing", async () => {
  const fixtureData = await fixture({ includeHelper: false });
  try {
    const result = runHelper(fixtureData);
    assert.notEqual(result.status, 0);
    assert.match(result.output, /code-mode host is missing/);
  } finally {
    await cleanup(fixtureData);
  }
});

test("fails loudly when the package manifest is missing", async () => {
  const fixtureData = await fixture();
  try {
    await rm(join(fixtureData.packageRoot, "codex-package.json"));
    const result = runHelper(fixtureData);
    assert.notEqual(result.status, 0);
    assert.match(result.output, /package manifest not found/);
  } finally {
    await cleanup(fixtureData);
  }
});

test("fails loudly when the pinned version does not match", async () => {
  const fixtureData = await fixture({ manifestVersion: "0.150.0" });
  try {
    const result = runHelper(fixtureData);
    assert.notEqual(result.status, 0);
    assert.match(result.output, /does not match expected 0\.151\.0/);
  } finally {
    await cleanup(fixtureData);
  }
});
