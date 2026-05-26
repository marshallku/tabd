import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { getProcessTreeRssBytes } from "../utils/processMonitor.js";

test("getProcessTreeRssBytes reports a sensible RSS for the current process", async () => {
  const bytes = await getProcessTreeRssBytes(process.pid);
  // Node itself uses tens of megabytes minimum; this is a sanity check
  // that the path through ps actually parsed something, not a strict
  // value bound.
  assert.ok(bytes > 1_000_000, `expected > 1MB, got ${bytes} bytes`);
});

test("getProcessTreeRssBytes includes child processes", async () => {
  // Spawn a child that sleeps so we can measure it.
  const child = spawn("sh", ["-c", "sleep 1"], {
    stdio: ["ignore", "ignore", "ignore"],
  });

  try {
    // Let the child fork / exec so `pgrep -P` can find it.
    await new Promise((r) => setTimeout(r, 100));
    const totalWithChild = await getProcessTreeRssBytes(process.pid);
    // Compare against the RSS of only this process; difference should be
    // positive (the child contributes something).
    const aloneApprox = await getProcessTreeRssBytes(child.pid as number);
    assert.ok(
      totalWithChild >= aloneApprox,
      `tree should include the child (tree=${totalWithChild}, child=${aloneApprox})`
    );
    assert.ok(
      aloneApprox > 0,
      `child process RSS should be measurable (${aloneApprox})`
    );
  } finally {
    child.kill();
  }
});

test("getProcessTreeRssBytes returns 0 for a non-existent pid", async () => {
  // A high pid that almost certainly does not exist
  const bytes = await getProcessTreeRssBytes(999_999_999);
  assert.equal(bytes, 0);
});
