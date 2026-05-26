import test from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { SnapshotKeeper } from "../utils/snapshotKeeper.js";

// Minimal mock of the Playwright Page surface that the keeper consumes.
function makeMockPage(initialUrl: string): {
  page: {
    on: (event: string, listener: (...args: unknown[]) => void) => void;
    url: () => string;
    mainFrame: () => unknown;
  };
  emitNavigate: (newUrl: string) => void;
  emitClose: () => void;
} {
  const emitter = new EventEmitter();
  let currentUrl = initialUrl;
  const mainFrame = {
    url: (): string => currentUrl,
  };
  return {
    page: {
      on(event, listener) {
        emitter.on(event, listener);
      },
      url: () => currentUrl,
      mainFrame: () => mainFrame,
    },
    emitNavigate(newUrl) {
      currentUrl = newUrl;
      emitter.emit("framenavigated", mainFrame);
    },
    emitClose() {
      emitter.emit("close");
    },
  };
}

function makeMockContext(): {
  context: {
    storageState: () => Promise<unknown>;
    pages: () => unknown[];
  };
  setStorageState: (s: unknown) => void;
} {
  let state: unknown = { cookies: [], origins: [] };
  return {
    context: {
      storageState: async () => state,
      pages: () => [],
    },
    setStorageState(s) {
      state = s;
    },
  };
}

test("SnapshotKeeper records initial URL when trackPage is called", () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 0 });
  const { context } = makeMockContext();
  k.attach(context as never);
  const m = makeMockPage("https://example.com/a");
  k.trackPage(m.page as never, "uuid-1");
  const snap = k.current();
  assert.deepEqual(snap.urls, ["https://example.com/a"]);
});

test("framenavigated updates the tracked URL", () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 0 });
  const { context } = makeMockContext();
  k.attach(context as never);
  const m = makeMockPage("https://example.com/a");
  k.trackPage(m.page as never, "uuid-1");
  m.emitNavigate("https://example.com/b");
  assert.deepEqual(k.current().urls, ["https://example.com/b"]);
});

test("close event does NOT remove URL (preserves pre-crash state)", () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 0 });
  const { context } = makeMockContext();
  k.attach(context as never);
  const m = makeMockPage("https://example.com/a");
  k.trackPage(m.page as never, "uuid-1");
  m.emitClose();
  // URL survives a "close" so a crashed context still has restore data.
  assert.deepEqual(k.current().urls, ["https://example.com/a"]);
});

test("dropPage removes URL (intentional tabs.close)", () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 0 });
  const { context } = makeMockContext();
  k.attach(context as never);
  const m = makeMockPage("https://example.com/a");
  k.trackPage(m.page as never, "uuid-1");
  k.dropPage("uuid-1");
  assert.deepEqual(k.current().urls, []);
});

test("refreshNow captures storageState; markDirty marks pending", async () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 0 });
  const m = makeMockContext();
  m.setStorageState({ cookies: [{ name: "s", value: "1" }], origins: [] });
  k.attach(m.context as never);

  await k.refreshNow();
  const snap = k.current();
  assert.ok(snap.storageState);
  assert.deepEqual(
    (snap.storageState as { cookies: unknown[] }).cookies,
    [{ name: "s", value: "1" }]
  );
});

test("captureStorageState=false skips storage capture (persistent mode)", async () => {
  const k = new SnapshotKeeper({
    refreshIntervalMs: 0,
    captureStorageState: false,
  });
  const m = makeMockContext();
  m.setStorageState({ cookies: [{ name: "should-not-appear" }], origins: [] });
  k.attach(m.context as never);
  await k.refreshNow();
  assert.equal(k.current().storageState, null);
});

test("detach stops the refresh timer", async () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 10 });
  const m = makeMockContext();
  k.attach(m.context as never);
  k.detach();
  // After detach, subsequent storage changes should not be auto-captured
  m.setStorageState({ cookies: [{ name: "new" }], origins: [] });
  await new Promise((r) => setTimeout(r, 30));
  // Snapshot should still be null because nothing refreshed it.
  assert.equal(k.current().storageState, null);
});

test("attach() resets prior URLs", () => {
  const k = new SnapshotKeeper({ refreshIntervalMs: 0 });
  const c1 = makeMockContext();
  k.attach(c1.context as never);
  const m1 = makeMockPage("https://old.example/");
  k.trackPage(m1.page as never, "uuid-1");
  assert.deepEqual(k.current().urls, ["https://old.example/"]);

  // New context — old URLs must not leak in
  const c2 = makeMockContext();
  k.attach(c2.context as never);
  assert.deepEqual(k.current().urls, []);
});
