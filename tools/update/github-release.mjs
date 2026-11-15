#!/usr/bin/env node
// 发布到 GitHub Releases（唯一对外更新通道）。
//
// 一条命令完成：生成 feed → 建 tag → 建 Release → 上传安装包、签名、latest.json。
// 客户端固定读 `releases/latest/download/latest.json`，所以不需要改客户端地址。
//
// 用法：
//   npm run release:github -- --notes "修了什么"
//   npm run release:github -- --notes-file CHANGELOG.md --draft
//
// 需要环境变量 GITHUB_TOKEN（repo 权限）。私钥仍由 tauri build 使用，
// 本脚本只上传已签名的产物，绝不读取或上传私钥。

import { execFileSync } from "node:child_process";
import { readFile, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");
const REPO_SLUG = "yefuchen1/idm-shadow";
const API = "https://api.github.com";

function parseArgs(argv) {
  const args = { notes: "", notesFile: "", draft: false, prerelease: false };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${key} 缺参数`);
      return argv[i];
    };
    if (key === "--notes") args.notes = next();
    else if (key === "--notes-file") args.notesFile = resolve(next());
    else if (key === "--draft") args.draft = true;
    else if (key === "--prerelease") args.prerelease = true;
    else if (key === "--help" || key === "-h") args.help = true;
    else throw new Error(`未知参数: ${key}`);
  }
  return args;
}

function token() {
  const value = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  if (!value) {
    throw new Error(
      "缺少 GITHUB_TOKEN。先在 GitHub 建一个 repo 权限的 token，再设到环境变量。"
    );
  }
  return value;
}

async function api(path, init = {}) {
  const response = await fetch(`${API}${path}`, {
    ...init,
    headers: {
      accept: "application/vnd.github+json",
      authorization: `Bearer ${token()}`,
      "x-github-api-version": "2022-11-28",
      ...(init.headers ?? {})
    }
  });
  const text = await response.text();
  const body = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new Error(
      `${init.method ?? "GET"} ${path} -> ${response.status}: ${
        body?.message ?? text
      }`
    );
  }
  return body;
}

function run(cmd, args, options = {}) {
  return execFileSync(cmd, args, {
    cwd: repo,
    stdio: "inherit",
    ...options
  });
}

async function uploadAsset(uploadUrl, file) {
  const name = file.split(/[\\/]/).pop();
  const size = (await stat(file)).size;
  console.log(`上传 ${name} (${(size / 1024 / 1024).toFixed(1)} MB)`);
  const response = await fetch(`${uploadUrl}?name=${encodeURIComponent(name)}`, {
    method: "POST",
    headers: {
      accept: "application/vnd.github+json",
      authorization: `Bearer ${token()}`,
      "content-type": "application/octet-stream",
      "content-length": String(size),
      "x-github-api-version": "2022-11-28"
    },
    body: await readFile(file)
  });
  if (!response.ok) {
    throw new Error(
      `上传 ${name} 失败: ${response.status} ${await response.text()}`
    );
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(
      "npm run release:github -- [--notes <text> | --notes-file <path>] [--draft] [--prerelease]"
    );
    return;
  }

  const version = JSON.parse(
    await readFile(join(repo, "app/tauri.conf.json"), "utf8")
  ).version;
  const tag = `v${version}`;
  const notes = args.notesFile
    ? (await readFile(args.notesFile, "utf8")).trim()
    : args.notes;
  if (!notes.trim()) {
    throw new Error("更新说明不能为空：发出去的每个版本都要告诉用户改了什么。");
  }

  const status = execFileSync("git", ["status", "--porcelain"], {
    cwd: repo,
    encoding: "utf8"
  });
  if (status.trim()) {
    throw new Error(
      "工作区有未提交改动。先提交并推送源码，再发版；否则 tag 指向的代码和安装包对不上。"
    );
  }

  if (execFileSync("git", ["tag", "--list", tag], { cwd: repo, encoding: "utf8" }).trim()) {
    throw new Error(`tag ${tag} 已存在。版本号没改就不要重复发版。`);
  }

  const baseUrl = `https://github.com/${REPO_SLUG}/releases/download/${tag}`;
  run("node", ["tools/update/publish.mjs", "--base-url", baseUrl, "--notes", notes]);

  const distDir = join(repo, "dist/updates");
  const feed = await readFile(join(distDir, "latest.json"), "utf8");
  const installer = JSON.parse(feed).platforms["windows-x86_64"].url.split("/").pop();

  run("git", ["tag", "-a", tag, "-m", `IDM Shadow ${version}`]);
  run("git", ["push", "origin", `refs/tags/${tag}`]);

  console.log(`创建 Release ${tag}`);
  const release = await api(`/repos/${REPO_SLUG}/releases`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      tag_name: tag,
      name: `IDM Shadow ${version}`,
      body: notes,
      draft: args.draft,
      prerelease: args.prerelease
    })
  });

  // 顺序有讲究：latest.json 最后传。先传就会有一小段时间
  // 客户端能读到新 feed、却下不到安装包。
  await uploadAsset(
    release.upload_url.replace("{?name,label}", ""),
    join(distDir, decodeURIComponent(installer))
  );
  await uploadAsset(
    release.upload_url.replace("{?name,label}", ""),
    join(repo, "target/release/bundle/nsis", `${decodeURIComponent(installer)}.sig`)
  );
  // latest.json 最后传：先传的话会有一小段时间客户端读到新 feed、
  // 却下不到安装包。
  await uploadAsset(
    release.upload_url.replace("{?name,label}", ""),
    join(repo, "dist/updates/latest.json")
  );

  console.log("");
  console.log(`发布完成: ${release.html_url}`);
  console.log(`更新源:   https://github.com/${REPO_SLUG}/releases/latest/download/latest.json`);
}

main().catch((err) => {
  console.error(`发版失败: ${err.message}`);
  process.exitCode = 1;
});
