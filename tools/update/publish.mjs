#!/usr/bin/env node
// 更新包发布工具。
//
// 所有对外更新都必须经过这里：读取 Tauri 打出来的安装包和 `.sig`，
// 生成 updater 的静态 feed（latest.json），再拷进一个可以直接托管的目录。
// 手工拷 exe 不算发布——没有 feed 和签名，客户端不会认。
//
// 用法：
//   node tools/update/publish.mjs --out dist/updates
//   node tools/update/publish.mjs --notes "修了什么" --out dist/updates
//
// 产物：
//   <out>/latest.json
//   <out>/IDM Shadow_0.2.0_x64-setup.exe

import { createHash } from "node:crypto";
import {
  copyFile,
  mkdir,
  readFile,
  readdir,
  stat,
  writeFile
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");
const nsisDir = join(repo, "target/release/bundle/nsis");
const REPO_SLUG = "yefuchen1/idm-shadow";

function parseArgs(argv) {
  const args = {
    out: join(repo, "dist/updates"),
    notes: "",
    baseUrl: "",
    notesFile: ""
  };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${key} 缺参数`);
      return argv[i];
    };
    if (key === "--out") args.out = resolve(next());
    else if (key === "--notes") args.notes = next();
    else if (key === "--notes-file") args.notesFile = resolve(next());
    else if (key === "--base-url") args.baseUrl = next().replace(/\/+$/, "");
    else if (key === "--help" || key === "-h") args.help = true;
    else throw new Error(`未知参数: ${key}`);
  }
  return args;
}

async function findInstaller(version) {
  let entries;
  try {
    entries = await readdir(nsisDir);
  } catch {
    throw new Error(
      `找不到 ${nsisDir}。先跑 npm run app:installer 打出安装包。`
    );
  }
  // 只认当前版本：构建目录里可能还留着旧版本的安装包，
  // 按名字排序挑最新的会悄悄发出过期的包。
  const setup = entries
    .filter(
      (name) =>
        name.endsWith(".exe") &&
        name.includes("setup") &&
        name.includes(`_${version}_`)
    )
    .sort();
  if (setup.length === 0) {
    throw new Error(
      `${nsisDir} 里没有 ${version} 的 *setup.exe。先跑 npm run app:installer。`
    );
  }
  // 只认带签名的那个：没有 .sig 说明构建时没配私钥，
  // 这种包发出去客户端也装不上。
  for (const name of setup.reverse()) {
    const sigPath = join(nsisDir, `${name}.sig`);
    try {
      await stat(sigPath);
      return { installer: join(nsisDir, name), sigPath, name };
    } catch {
      // 继续找上一个
    }
  }
  throw new Error(
    `${version} 的安装包旁边没有 .sig。构建时缺 TAURI_SIGNING_PRIVATE_KEY，更新包无法被信任。`
  );
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(
      "node tools/update/publish.mjs [--out <dir>] [--notes <text> | --notes-file <path>] [--base-url <url>]"
    );
    return;
  }

  const version = JSON.parse(
    await readFile(join(repo, "app/tauri.conf.json"), "utf8")
  ).version;
  // 界面显示的版本来自 CARGO_PKG_VERSION（workspace 版本号），
  // 两者不一致会让客户端拿错版本基线，检查更新直接失效。
  const cargoToml = await readFile(join(repo, "Cargo.toml"), "utf8");
  const cargoVersion = cargoToml.match(
    /\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m
  )?.[1];
  if (cargoVersion !== version) {
    throw new Error(
      `版本号不一致：Cargo.toml=${cargoVersion ?? "?"}，tauri.conf.json=${version}`
    );
  }
  const { installer, sigPath, name } = await findInstaller(version);
  const notes = args.notesFile
    ? (await readFile(args.notesFile, "utf8")).trim()
    : args.notes;
  const signature = (await readFile(sigPath, "utf8")).trim();
  const bytes = await readFile(installer);
  const sha256 = createHash("sha256").update(bytes).digest("hex");

  await mkdir(args.out, { recursive: true });
  const target = join(args.out, name);
  await copyFile(installer, target);

  const url = args.baseUrl ? `${args.baseUrl}/${encodeURI(name)}` : name;
  const feed = {
    version,
    notes,
    pub_date: new Date().toISOString(),
    platforms: {
      "windows-x86_64": {
        signature,
        url
      }
    }
  };
  const feedPath = join(args.out, "latest.json");
  await writeFile(feedPath, `${JSON.stringify(feed, null, 2)}\n`, "utf8");

  console.log(`版本    ${version}`);
  console.log(`安装包  ${name}`);
  console.log(`大小    ${(bytes.length / 1024 / 1024).toFixed(1)} MB`);
  console.log(`SHA256  ${sha256}`);
  console.log(`feed    ${feedPath}`);
  if (!args.baseUrl) {
    console.log("");
    console.log(
      "注意：没给 --base-url，feed 里的 url 是相对路径。" +
        "发到 GitHub 请用 npm run release:github，它会自动带上正确地址。"
    );
  }
}

// 作为模块被 github-release.mjs 引用时不要自动执行。
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((err) => {
    console.error(`发布失败: ${err.message}`);
    process.exitCode = 1;
  });
}
