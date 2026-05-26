// processMonitor — query RSS of a process tree using POSIX `ps`/`pgrep`.
//
// Why not pidusage? It would add a native dep with platform builds. The
// daemon already shells out for keychain/secret-tool; one more `ps` call
// every 15 seconds is negligible. ps with `-o rss=` works identically on
// Linux and macOS and reports RSS in KB.
//
// `pgrep -P <pid>` lists children. Chromium spawns renderer / GPU / utility
// children; their RSS is the bulk of memory usage and must be included in
// the total.

import { spawn } from "node:child_process";

interface SpawnResult {
  code: number;
  stdout: string;
}

function runQuiet(cmd: string, args: string[]): Promise<SpawnResult> {
  return new Promise((resolve) => {
    const proc = spawn(cmd, args, { stdio: ["ignore", "pipe", "ignore"] });
    let out = "";
    proc.stdout.setEncoding("utf8");
    proc.stdout.on("data", (chunk: string) => {
      out += chunk;
    });
    proc.on("error", () => resolve({ code: -1, stdout: out }));
    proc.on("close", (code) => resolve({ code: code ?? -1, stdout: out }));
  });
}

async function listChildren(pid: number): Promise<number[]> {
  const { code, stdout } = await runQuiet("pgrep", ["-P", String(pid)]);
  if (code !== 0) return [];
  return stdout
    .split("\n")
    .map((line) => Number.parseInt(line.trim(), 10))
    .filter((n) => Number.isFinite(n) && n > 0);
}

async function collectTree(rootPid: number): Promise<number[]> {
  const seen = new Set<number>([rootPid]);
  const queue = [rootPid];
  while (queue.length > 0) {
    const current = queue.shift() as number;
    const children = await listChildren(current);
    for (const child of children) {
      if (seen.has(child)) continue;
      seen.add(child);
      queue.push(child);
    }
  }
  return Array.from(seen);
}

async function rssKbForPids(pids: number[]): Promise<number> {
  if (pids.length === 0) return 0;
  // `ps -o rss= -p <comma-separated>` returns RSS in kilobytes, one line per
  // pid. Missing pids are silently skipped (race-safe).
  const { code, stdout } = await runQuiet("ps", [
    "-o",
    "rss=",
    "-p",
    pids.join(","),
  ]);
  if (code !== 0) return 0;
  return stdout
    .split("\n")
    .map((line) => Number.parseInt(line.trim(), 10))
    .filter((n) => Number.isFinite(n))
    .reduce((acc, kb) => acc + kb, 0);
}

/**
 * Returns total RSS of `rootPid` and all transitive children, in bytes.
 * Returns 0 if the tree could not be measured (e.g. process already gone
 * or `ps` not available). Never throws — the caller treats unknown as 0.
 */
export async function getProcessTreeRssBytes(rootPid: number): Promise<number> {
  try {
    const tree = await collectTree(rootPid);
    const kb = await rssKbForPids(tree);
    return kb * 1024;
  } catch {
    return 0;
  }
}
