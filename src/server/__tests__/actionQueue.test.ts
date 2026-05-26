import test from "node:test";
import assert from "node:assert/strict";
import { setTimeout as delay } from "node:timers/promises";
import { ActionQueue, GLOBAL } from "../utils/actionQueue.js";

test("same scope runs serially", async () => {
  const q = new ActionQueue();
  const events: string[] = [];
  const a = q.enqueue("tab-1", async () => {
    events.push("a-start");
    await delay(30);
    events.push("a-end");
    return "a";
  });
  const b = q.enqueue("tab-1", async () => {
    events.push("b-start");
    return "b";
  });
  assert.equal(await a, "a");
  assert.equal(await b, "b");
  assert.deepEqual(events, ["a-start", "a-end", "b-start"]);
});

test("different scopes run concurrently", async () => {
  const q = new ActionQueue();
  const events: string[] = [];
  const a = q.enqueue("tab-1", async () => {
    events.push("a-start");
    await delay(40);
    events.push("a-end");
    return "a";
  });
  const b = q.enqueue("tab-2", async () => {
    events.push("b-start");
    await delay(10);
    events.push("b-end");
    return "b";
  });
  await Promise.all([a, b]);
  // b should finish before a even though it was enqueued second
  const aEnd = events.indexOf("a-end");
  const bEnd = events.indexOf("b-end");
  assert.ok(bEnd < aEnd, `b-end (${bEnd}) should precede a-end (${aEnd})`);
});

test("global locks out all scopes until it completes", async () => {
  const q = new ActionQueue();
  const events: string[] = [];

  const t1 = q.enqueue("tab-1", async () => {
    events.push("t1-start");
    await delay(20);
    events.push("t1-end");
  });

  const g = q.enqueue(GLOBAL, async () => {
    events.push("g-start");
    await delay(20);
    events.push("g-end");
  });

  const t2 = q.enqueue("tab-2", async () => {
    events.push("t2-start");
  });

  await Promise.all([t1, g, t2]);

  // t1 was already enqueued before g, so it can overlap.
  // g must end before t2 starts (global lock).
  const gEnd = events.indexOf("g-end");
  const t2Start = events.indexOf("t2-start");
  assert.ok(gEnd < t2Start, `g-end (${gEnd}) must precede t2-start (${t2Start})`);
});

test("rejection in one action does not break the chain", async () => {
  const q = new ActionQueue();
  const failed = q.enqueue("tab-1", async () => {
    throw new Error("boom");
  });
  const after = q.enqueue("tab-1", async () => "after");
  await assert.rejects(() => failed, /boom/);
  assert.equal(await after, "after");
});

test("idle scopes are garbage collected", async () => {
  const q = new ActionQueue();
  await q.enqueue("tab-1", async () => undefined);
  // Give the GC microtask a tick to run
  await delay(5);
  assert.equal(q.inflightScopeCount(), 0);
});
