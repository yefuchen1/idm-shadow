#!/usr/bin/env node
// 本地更新源，用于自测和局域网内分发。
//
// 用法：
//   npm run update:serve            # 默认 serve dist/updates，端口 8787
//   node tools/update/serve.mjs --dir dist/updates --port 8787
//
// 注意：正式版本的 updater 只接受 HTTPS。这里起的是 HTTP，
// 只适合 debug 构建或临时用 --config 打开 dangerousInsecureTransportProtocol
// 的测试包。别把这种测试包发给别人。

import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { createServer } from "node:http";
import { networkInterfaces } from "node:os";
import { dirname, extname, join, normalize, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");

const MIME = {
  ".json": "application/json; charset=utf-8",
  ".exe": "application/octet-stream",
  ".msi": "application/octet-stream",
  ".sig": "text/plain; charset=utf-8",
  ".txt": "text/plain; charset=utf-8",
  ".html": "text/html; charset=utf-8"
};

function parseArgs(argv) {
  const args = { dir: join(repo, "dist/updates"), port: 8787, host: "0.0.0.0" };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${key} 缺参数`);
      return argv[i];
    };
    if (key === "--dir") args.dir = resolve(next());
    else if (key === "--port") args.port = Number(next());
    else if (key === "--host") args.host = next();
    else if (key === "--help" || key === "-h") args.help = true;
    else throw new Error(`未知参数: ${key}`);
  }
  if (!Number.isInteger(args.port) || args.port <= 0 || args.port > 65535) {
    throw new Error(`端口不合法: ${args.port}`);
  }
  return args;
}

function lanAddresses() {
  const out = [];
  for (const list of Object.values(networkInterfaces())) {
    for (const net of list ?? []) {
      if (net.family === "IPv4" && !net.internal) out.push(net.address);
    }
  }
  return out;
}

async function resolveFile(root, rawUrl) {
  const pathname = decodeURIComponent(new URL(rawUrl, "http://x").pathname);
  const candidate = normalize(join(root, pathname));
  // 防目录穿越：拼接后的绝对路径必须还在 root 里
  if (candidate !== root && !candidate.startsWith(root + sep)) return null;
  try {
    const info = await stat(candidate);
    if (info.isFile()) return { path: candidate, size: info.size };
  } catch {
    // 不存在就当 404
  }
  return null;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(
      "node tools/update/serve.mjs [--dir <dir>] [--port <n>] [--host <addr>]"
    );
    return;
  }

  const root = resolve(args.dir);
  const feed = join(root, "latest.json");
  try {
    await stat(feed);
  } catch {
    throw new Error(
      `找不到 ${feed}。先跑 npm run update:publish -- --base-url <这台机器的地址>。`
    );
  }

  const server = createServer(async (req, res) => {
    const file = await resolveFile(root, req.url ?? "/");
    if (!file) {
      res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
      res.end("not found");
      return;
    }
    res.writeHead(200, {
      "content-type": MIME[extname(file.path).toLowerCase()] ?? "application/octet-stream",
      "content-length": file.size,
      "cache-control": "no-store"
    });
    createReadStream(file.path).pipe(res);
  });

  server.listen(args.port, args.host, () => {
    console.log(`更新源目录  ${root}`);
    console.log(`本机        http://127.0.0.1:${args.port}/latest.json`);
    for (const ip of lanAddresses()) {
      console.log(`局域网      http://${ip}:${args.port}/latest.json`);
    }
    console.log("");
    console.log("把上面某个 latest.json 地址填进应用设置里的「更新源」。");
    console.log("HTTP 只用于测试；正式分发包必须走 HTTPS。");
  });
}

main().catch((err) => {
  console.error(`启动失败: ${err.message}`);
  process.exitCode = 1;
});
