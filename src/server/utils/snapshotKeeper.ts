// SnapshotKeeper — continuously maintains an in-memory snapshot of the
// browser context so the supervisor can restore approximately the same
// state after a Chromium crash. State is captured BEFORE the crash because
// the dying context cannot be queried after it goes down.
//
// Two pieces of state are tracked:
//   1. URL list (per page UUID) — kept up to date via framenavigated events
//   2. storageState (cookies + localStorage) — refreshed periodically and
//      when storage-mutating actions complete, on a "dirty" flag
//
// Scope: snapshot data lives in process memory only. It is never written
// to disk — Chromium's own profile (persistent mode) is the long-term
// store. This module is concerned with bridging a single crash, not
// surviving daemon restarts.

import type { BrowserContext, Page } from "playwright-core";

export interface ContextSnapshot {
  urls: string[];
  // Playwright storageState shape (cookies + localStorage). Kept opaque
  // here; the consumer passes it back into newContext({ storageState }).
  storageState: unknown | null;
  takenAt: number;
}

export interface SnapshotKeeperOptions {
  /** Refresh storageState after this many ms when dirty. Default 5_000. */
  refreshIntervalMs?: number;
  /** Set true to capture storageState. Persistent contexts pass false. */
  captureStorageState?: boolean;
}

export class SnapshotKeeper {
  private readonly urls = new Map<string /* pageUuid */, string>();
  private storageState: unknown | null = null;
  private dirty = false;
  private refreshTimer: ReturnType<typeof setInterval> | null = null;
  private context: BrowserContext | null = null;
  private readonly captureStorageState: boolean;
  private readonly refreshIntervalMs: number;
  // Used to drop stale framenavigated updates after detach()
  private detached = false;

  constructor(options: SnapshotKeeperOptions = {}) {
    this.captureStorageState = options.captureStorageState ?? true;
    this.refreshIntervalMs = options.refreshIntervalMs ?? 5_000;
  }

  /**
   * Attach to a fresh context. Replaces any prior URL list. Optional
   * `initialStorageState` seeds the in-memory snapshot — used after a
   * crash restart where the new context was created from a known
   * storageState; without seeding, a second crash before the first
   * action+refresh cycle would lose that state again.
   */
  attach(
    context: BrowserContext,
    options?: { initialStorageState?: unknown }
  ): void {
    this.detach();
    this.detached = false;
    this.context = context;
    this.urls.clear();
    this.storageState = options?.initialStorageState ?? null;
    this.dirty = false;
    if (this.captureStorageState && this.refreshIntervalMs > 0) {
      this.refreshTimer = setInterval(
        () => void this.refreshIfDirty(),
        this.refreshIntervalMs
      );
      // Don't keep the event loop alive just for the refresh tick.
      (this.refreshTimer as { unref?: () => void }).unref?.();
    }
  }

  /** Stop background work; called before context.close() during shutdown. */
  detach(): void {
    this.detached = true;
    if (this.refreshTimer) {
      clearInterval(this.refreshTimer);
      this.refreshTimer = null;
    }
    this.context = null;
  }

  /**
   * Register a page so its URL is tracked. Called on context "page" event
   * AND on initial attach for already-open pages.
   */
  trackPage(page: Page, uuid: string): void {
    this.urls.set(uuid, page.url());
    page.on("framenavigated", (frame) => {
      if (this.detached) return;
      if (frame === page.mainFrame()) {
        this.urls.set(uuid, frame.url());
      }
    });
    page.on("close", () => {
      // Don't drop the URL on close — if Chromium crashed, every page
      // emits "close" before we can react. We want the pre-crash URL
      // for restore. Pages closed via tabs.close are removed via
      // dropPage() instead.
    });
  }

  /** Remove a page from tracking after an intentional tabs.close. */
  dropPage(uuid: string): void {
    this.urls.delete(uuid);
    this.markDirty();
  }

  /** Mark storageState as needing refresh on the next tick. */
  markDirty(): void {
    this.dirty = true;
  }

  /** Read the current snapshot for restore. Returns a defensive copy. */
  current(): ContextSnapshot {
    return {
      urls: Array.from(this.urls.values()),
      // structuredClone so a caller that mutates storageState (e.g. while
      // restoring) cannot corrupt the keeper's restore payload for a
      // subsequent crash.
      storageState:
        this.storageState !== null
          ? structuredClone(this.storageState)
          : null,
      takenAt: Date.now(),
    };
  }

  /**
   * Force a synchronous refresh — useful right before a planned restart
   * so the freshest possible state is captured. No-op if there is no
   * context attached.
   */
  async refreshNow(): Promise<void> {
    if (!this.context || !this.captureStorageState) return;
    try {
      this.storageState = await this.context.storageState();
      this.dirty = false;
    } catch {
      // Best-effort; the context might already be tearing down.
    }
  }

  private async refreshIfDirty(): Promise<void> {
    if (!this.dirty) return;
    await this.refreshNow();
  }
}
