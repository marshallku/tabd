import { existsSync } from "node:fs";
import { randomBytes } from "node:crypto";
import type { ChildProcess } from "node:child_process";
import {
  chromium,
  firefox,
  webkit,
  type Browser,
  type BrowserContext,
  type BrowserServer,
  type BrowserType,
  type ConsoleMessage,
  type Dialog,
  type LaunchOptions,
  type Page,
  type Request,
  type Response,
} from "playwright-core";
import { unlink as fsUnlink } from "node:fs/promises";
import { join as pathJoin } from "node:path";
import type { BridgeAction, BridgeResponse } from "../../shared/protocol.js";
import type { BrowserDriver } from "../bridge.js";
import { getSecretStore } from "../secrets.js";
import { compileUrlMatcher, type UrlPatternType } from "../../shared/urlMatch.js";
import { ActionQueue, GLOBAL, type Scope } from "../utils/actionQueue.js";
import { SnapshotKeeper, type ContextSnapshot } from "../utils/snapshotKeeper.js";
import { getProcessTreeRssBytes } from "../utils/processMonitor.js";

interface PlaywrightOptions {
  browserName: string;
  executablePath?: string;
  userDataDir?: string;
  headless: boolean;
  startupTimeoutMs: number;
  viewportWidth: number;
  viewportHeight: number;
  keepaliveIntervalMs?: number;
}

interface NetworkEntry {
  id: string;
  url: string;
  method: string;
  resourceType: string;
  status: number | null;
  statusText: string | null;
  requestHeaders: Record<string, string>;
  responseHeaders: Record<string, string>;
  requestBody: string | null;
  responseBody: string | null;
  responseBodyTruncated: boolean;
  responseBodySize: number | null;
  startTime: number;
  endTime: number | null;
  durationMs: number | null;
  fromCache: boolean;
  failed: boolean;
  failureText: string | null;
}

interface PageState {
  consoleLogs: Array<Record<string, unknown>>;
  pageErrors: Array<Record<string, unknown>>;
  networkLogs: NetworkEntry[];
  requestIndex: Map<Request, NetworkEntry>;
  dialogBehavior: { action: "accept" | "dismiss"; text?: string };
  lastDialog: Record<string, unknown> | null;
}

// Sentinel value for __pageUuid when an explicit tabId could not be
// resolved at enqueue time. Dispatch detects this and raises a clean
// "Tab closed before action could execute" error instead of letting the
// positional tabId silently re-resolve to a different page later on.
const UNRESOLVED_TAB = "__unresolved__";

// Maximum consecutive restart attempts before giving up and exiting the
// daemon process. Process supervisors (systemd, launchd) can then do a
// fresh boot. Backoff grows exponentially: 1s, 2s, 4s, capped at 30s.
const MAX_RESTART_ATTEMPTS = 3;
const RESTART_BACKOFF_CAP_MS = 30_000;

// How often to poll the Chromium process tree for RSS. 15s is a balance
// between responsiveness to leaks and overhead of fork(`ps`/`pgrep`).
const RSS_POLL_INTERVAL_MS = 15_000;

// Chromium leaves these single-instance lock files in the user-data-dir
// after an unclean exit. The next launchPersistentContext will fail with
// "ProcessSingleton" or similar until they are removed.
const CHROMIUM_LOCK_FILES = [
  "SingletonLock",
  "SingletonCookie",
  "SingletonSocket",
];

async function clearChromiumLocks(userDir: string): Promise<void> {
  await Promise.all(
    CHROMIUM_LOCK_FILES.map(async (name) => {
      try {
        await fsUnlink(pathJoin(userDir, name));
      } catch (err) {
        const code = (err as NodeJS.ErrnoException).code;
        if (code === "ENOENT") {
          // Missing is fine — clean shutdown leaves no locks.
          return;
        }
        // Anything else (permission, IO error) deserves attention because
        // launchPersistentContext will otherwise fail with a confusing
        // ProcessSingleton error and we'd lose the original cause.
        console.error(
          `[playwright] failed to clear lock ${name} in ${userDir}:`,
          err
        );
      }
    })
  );
}

const MAX_LOG_ENTRIES = 200;
const MAX_NETWORK_ENTRIES = 500;
const MAX_BODY_BYTES = 100_000;
const TEXT_CONTENT_TYPES =
  /^(text\/|application\/(json|xml|javascript|x-www-form-urlencoded|graphql))/i;

export class PlaywrightBrowserDriver implements BrowserDriver {
  private readonly options: PlaywrightOptions;
  // Non-persistent path: server owns the Chromium child process and exposes
  // a ws endpoint; we connect a Browser to it. Persistent path skips the
  // server because launchPersistentContext does not expose one.
  private server: BrowserServer | null = null;
  private browser: Browser | null = null;
  private context: BrowserContext | null = null;
  private chromiumProc: ChildProcess | null = null;
  // True while close()/restart is intentionally tearing the runtime down.
  // Crash listeners read this to distinguish planned shutdown from an
  // unexpected exit so the future supervisor (Phase 5) does not relaunch
  // during normal teardown.
  private closingIntentionally = false;
  private activeTabId: number | null = null;
  private readonly pageStates = new Map<Page, PageState>();
  // Stable, position-independent page identity used for ActionQueue scope.
  // Client-facing tabId (positional index) can shift when other tabs close,
  // but queued work must still resolve against the same Page object.
  private readonly pageUuids = new WeakMap<Page, string>();
  private readonly secrets = getSecretStore();
  private readonly queue = new ActionQueue();
  private keepaliveTimer: ReturnType<typeof setInterval> | null = null;
  // Crash supervisor state
  private readonly snapshot: SnapshotKeeper;
  // Carried into init() so a restart can apply the last storageState to the
  // new context. Non-persistent path only.
  private pendingStorageState: unknown | null = null;
  // Carried into init() so a restart restores the URL list afterwards.
  private pendingUrls: string[] | null = null;
  // Number of consecutive failed restarts. Reset on a successful restart.
  // Hits MAX_RESTART_ATTEMPTS → die loudly so a process supervisor (systemd)
  // can do a fresh boot.
  private restartAttempt = 0;
  private restarting: Promise<void> | null = null;
  // Resource monitor state. lastRssBytes is reported via daemon.health.
  // When non-zero, BROWSER_MAX_RSS_MB env triggers a graceful restart on
  // threshold crossing.
  private rssTimer: ReturnType<typeof setInterval> | null = null;
  private lastRssBytes = 0;
  private lastRssAt = 0;

  constructor(options: PlaywrightOptions) {
    this.options = options;
    // Persistent contexts have their cookies/localStorage backed by the
    // userDataDir on disk; storageState injection is not their normal
    // restore path. Non-persistent contexts must capture storageState in
    // memory so a crash restart can re-apply it to the new context.
    this.snapshot = new SnapshotKeeper({
      captureStorageState: !options.userDataDir,
    });
  }

  async init(): Promise<void> {
    const browserType = this.resolveBrowserType();
    this.closingIntentionally = false;

    if (this.options.userDataDir) {
      // Persistent path: profile-backed context. BrowserServer is not
      // available here; crash signal is context.on("close"). On restart
      // after an unclean Chromium exit, leftover Singleton* lock files
      // would block relaunch — clear them first.
      await clearChromiumLocks(this.options.userDataDir);
      this.context = await browserType.launchPersistentContext(
        this.options.userDataDir,
        {
          headless: this.options.headless,
          executablePath: this.resolveExecutablePath(),
          timeout: this.options.startupTimeoutMs,
          viewport: {
            width: this.options.viewportWidth,
            height: this.options.viewportHeight,
          },
        }
      );
      this.browser = null;
      this.server = null;
      this.chromiumProc = null;
      this.context.on("close", () => this.onContextClosed());
    } else {
      // Non-persistent path: launchServer + connect. This is the only public
      // launch shape that exposes Chromium PID + exit events to the daemon,
      // which the Phase 5 supervisor needs for tree-RSS monitoring and
      // crash-vs-planned-close discrimination.
      //
      // Security: launchServer's default host is "::" / "0.0.0.0", which makes
      // the Chromium control WebSocket reachable from LAN. Pin to loopback and
      // randomize the wsPath so that another local user cannot discover and
      // hijack the endpoint by scanning ports.
      const launchServerOptions = {
        headless: this.options.headless,
        executablePath: this.resolveExecutablePath(),
        timeout: this.options.startupTimeoutMs,
        host: "127.0.0.1",
        wsPath: `/ai-browser/${randomBytes(16).toString("hex")}`,
      } satisfies Parameters<BrowserType["launchServer"]>[0];
      this.server = await browserType.launchServer(launchServerOptions);
      this.chromiumProc = this.server.process();
      this.server.on("close", () => this.onServerClosed());
      this.chromiumProc.on("exit", (code, signal) =>
        this.onChromiumExit(code, signal)
      );

      this.browser = await browserType.connect(this.server.wsEndpoint());
      // Apply storageState if a restart is replaying the prior session's
      // cookies/localStorage. pendingStorageState is set by the supervisor
      // right before it calls init() again.
      const contextOptions: Parameters<Browser["newContext"]>[0] = {
        viewport: {
          width: this.options.viewportWidth,
          height: this.options.viewportHeight,
        },
      };
      if (this.pendingStorageState) {
        (contextOptions as Record<string, unknown>).storageState =
          this.pendingStorageState;
      }
      this.context = await this.browser.newContext(contextOptions);
      // pendingStorageState is read once more below in snapshot.attach()
      // for the keeper seed. It is cleared AFTER that read so a second
      // crash before the next 5s refresh still has the cookies/storage.
    }

    this.context.on("page", (page) => this.attachPage(page));

    const pages = this.context.pages();
    const page = pages.length > 0 ? pages[0] : await this.context.newPage();
    this.attachPage(page);
    this.activeTabId = this.tabIdForPage(page);

    // Begin continuous snapshot tracking. The keeper records URLs in real
    // time via framenavigated events and periodically refreshes
    // storageState (non-persistent mode). The pre-crash snapshot is what
    // the supervisor restores after a Chromium kill. On a restart we MUST
    // seed the keeper with the storageState we just applied to the new
    // context, otherwise a back-to-back crash before the next refresh
    // cycle would lose cookies/localStorage.
    this.snapshot.attach(this.context, {
      initialStorageState: this.pendingStorageState ?? undefined,
    });
    // Now safe to clear — both the new context AND the keeper hold copies.
    this.pendingStorageState = null;
    for (const p of this.context.pages()) {
      const uuid = this.pageUuids.get(p);
      if (uuid) this.snapshot.trackPage(p, uuid);
    }

    // Restore URL list on the new context after a restart (the supervisor
    // sets pendingUrls before calling init() again). Done after snapshot
    // attach so the new URLs feed back into snapshot tracking immediately.
    if (this.pendingUrls && this.pendingUrls.length > 0) {
      const urlsToRestore = this.pendingUrls;
      this.pendingUrls = null;
      await this.restoreUrls(urlsToRestore);
    }

    this.startKeepalive();
    this.startRssMonitor();
  }

  /** Snapshot of supervisor + resource state for daemon.health. */
  getDriverHealth(): {
    chromiumPid: number | null;
    chromiumRssBytes: number;
    rssCheckedAt: number;
    rssMaxMb: number | null;
    restartAttempt: number;
    restarting: boolean;
  } {
    const rssMaxMbEnv = process.env.BROWSER_MAX_RSS_MB;
    const rssMaxMb = rssMaxMbEnv ? Number(rssMaxMbEnv) : null;
    return {
      chromiumPid: this.chromiumProc?.pid ?? null,
      chromiumRssBytes: this.lastRssBytes,
      rssCheckedAt: this.lastRssAt,
      rssMaxMb: rssMaxMb && Number.isFinite(rssMaxMb) ? rssMaxMb : null,
      restartAttempt: this.restartAttempt,
      restarting: this.restarting !== null,
    };
  }

  private startRssMonitor(): void {
    this.stopRssMonitor();
    // Persistent path has no Chromium PID available (launchPersistentContext
    // does not expose it), so tree RSS cannot be computed. Skip monitor —
    // operator must rely on OS-level tools in that case.
    if (!this.chromiumProc?.pid) return;
    const intervalEnv = process.env.BROWSER_RSS_POLL_MS;
    const intervalMs =
      intervalEnv && Number.isFinite(Number(intervalEnv))
        ? Math.max(500, Number(intervalEnv))
        : RSS_POLL_INTERVAL_MS;
    const tick = async (): Promise<void> => {
      const rootPid = this.chromiumProc?.pid;
      if (!rootPid) return;
      try {
        const bytes = await getProcessTreeRssBytes(rootPid);
        this.lastRssBytes = bytes;
        this.lastRssAt = Date.now();
        const cap = Number(process.env.BROWSER_MAX_RSS_MB ?? 0);
        if (
          Number.isFinite(cap) &&
          cap > 0 &&
          bytes / (1024 * 1024) > cap &&
          !this.restarting &&
          !this.closingIntentionally
        ) {
          console.error(
            `[playwright] RSS ${Math.round(bytes / 1024 / 1024)}MB exceeds BROWSER_MAX_RSS_MB=${cap}; scheduling graceful restart`
          );
          // Restart via the same supervisor as crash recovery. The pre-
          // restart snapshot keeps URLs + storageState, so the new process
          // resumes the prior session at lower RSS.
          void this.scheduleRestart("rss-threshold");
        }
      } catch {
        // Best-effort; transient ps failures are not actionable here.
      }
    };
    // Run one immediately so health reflects a real value quickly.
    void tick();
    this.rssTimer = setInterval(() => void tick(), intervalMs);
    (this.rssTimer as { unref?: () => void }).unref?.();
  }

  private stopRssMonitor(): void {
    if (this.rssTimer) {
      clearInterval(this.rssTimer);
      this.rssTimer = null;
    }
  }

  private async restoreUrls(urls: string[]): Promise<void> {
    if (!this.context) return;
    for (let i = 0; i < urls.length; i++) {
      const url = urls[i];
      if (!url || url.startsWith("about:") || url.startsWith("chrome-error:")) {
        continue;
      }
      let page: Page;
      const existing = this.context.pages();
      if (i < existing.length) {
        page = existing[i];
      } else {
        page = await this.context.newPage();
        this.attachPage(page);
        const uuid = this.pageUuids.get(page);
        if (uuid) this.snapshot.trackPage(page, uuid);
      }
      try {
        await page.goto(url, { waitUntil: "load", timeout: 30_000 });
      } catch (err) {
        console.error(
          `[playwright] restore: navigation to ${url} failed:`,
          err instanceof Error ? err.message : err
        );
      }
    }
  }

  async close(): Promise<void> {
    this.closingIntentionally = true;
    this.stopKeepalive();
    this.stopRssMonitor();
    // Stop the snapshot refresh tick BEFORE context.close so the timer
    // doesn't fire one last storageState query against a closing context.
    this.snapshot.detach();
    this.pageStates.clear();
    // Tear down in dependency order. Each step is best-effort so a partial
    // failure does not prevent cleanup of the next layer.
    try {
      await this.context?.close();
    } catch (err) {
      console.error("[playwright] context.close error:", err);
    }
    try {
      await this.browser?.close();
    } catch (err) {
      console.error("[playwright] browser.close error:", err);
    }
    try {
      await this.server?.close();
    } catch (err) {
      console.error("[playwright] server.close error:", err);
    }
    this.context = null;
    this.browser = null;
    this.server = null;
    this.chromiumProc = null;
    this.activeTabId = null;
  }

  // --- Crash / lifecycle observers + supervisor ---------------------------
  // Both unexpected close events route into scheduleRestart, which captures
  // the last known snapshot, tears the dead runtime down, and re-inits.
  private onServerClosed(): void {
    if (this.closingIntentionally) return;
    console.error("[playwright] BrowserServer closed unexpectedly");
    void this.scheduleRestart("server-closed");
  }

  private onChromiumExit(code: number | null, signal: NodeJS.Signals | null): void {
    if (this.closingIntentionally) return;
    console.error(
      `[playwright] Chromium process exited unexpectedly (code=${code}, signal=${signal ?? "-"})`
    );
    void this.scheduleRestart(`chromium-exit (code=${code})`);
  }

  private onContextClosed(): void {
    if (this.closingIntentionally) return;
    console.error(
      "[playwright] Persistent context closed unexpectedly (likely Chromium crash)"
    );
    void this.scheduleRestart("persistent-context-close");
  }

  private async scheduleRestart(reason: string): Promise<void> {
    if (this.closingIntentionally) return;
    // Coalesce concurrent crash signals (server-close + process-exit often
    // fire together) onto a single restart in-flight.
    if (this.restarting) return await this.restarting;

    if (this.restartAttempt >= MAX_RESTART_ATTEMPTS) {
      console.error(
        `[playwright] giving up after ${this.restartAttempt} consecutive restart attempts (reason: ${reason})`
      );
      // Let the process supervisor (systemd / launchd) start a fresh
      // daemon. exit(1) marks this as a failure for restart-on-failure.
      process.exit(1);
    }

    this.restartAttempt++;
    const backoffMs = Math.min(
      RESTART_BACKOFF_CAP_MS,
      1_000 * 2 ** (this.restartAttempt - 1)
    );
    console.error(
      `[playwright] restart attempt ${this.restartAttempt}/${MAX_RESTART_ATTEMPTS} in ${backoffMs}ms (reason: ${reason})`
    );

    this.restarting = (async (): Promise<void> => {
      try {
        await new Promise((resolve) => setTimeout(resolve, backoffMs));
        // Shutdown can be requested while we were sleeping. If so, bail
        // before we relaunch Chromium — otherwise the daemon would tear
        // down its socket but the new Chromium would still be alive.
        if (this.closingIntentionally) return;

        // Snapshot is captured pre-restart (it has the last good URLs +
        // storageState in memory).
        const snap: ContextSnapshot = this.snapshot.current();

        // Stop background timers before tearing the runtime down. Otherwise
        // a new interval is installed on top of the old one and they
        // accumulate per restart.
        this.stopKeepalive();
        this.stopRssMonitor();
        this.snapshot.detach();
        this.pageStates.clear();

        // Gracefully close the existing browser tree BEFORE the relaunch.
        // For crash recovery (server already exited) these are essentially
        // no-ops. For RSS-threshold restart they are the actual memory
        // reclaim — without them we would leave the old high-RSS Chromium
        // alive and spawn a second one. Each step is best-effort so a dead
        // pointer does not prevent the next layer from being cleaned up.
        try {
          await this.context?.close();
        } catch {
          /* may already be dead from crash */
        }
        try {
          await this.browser?.close();
        } catch {
          /* may already be dead */
        }
        try {
          await this.server?.close();
        } catch {
          /* may already be dead */
        }
        this.context = null;
        this.browser = null;
        this.server = null;
        this.chromiumProc = null;
        this.activeTabId = null;

        // Carry forward state for the new context.
        this.pendingStorageState = snap.storageState ?? null;
        this.pendingUrls = snap.urls.length > 0 ? snap.urls : null;

        // Re-check the gate one more time: between the snapshot capture
        // and starting init() the daemon could have observed shutdown.
        if (this.closingIntentionally) return;
        await this.init();
        // If shutdown arrived during init() itself, tear the new context
        // back down so we don't leave Chromium running with no daemon.
        if (this.closingIntentionally) {
          await this.close().catch(() => undefined);
          return;
        }
        console.error(
          `[playwright] restart attempt ${this.restartAttempt} succeeded`
        );
        this.restartAttempt = 0;
      } catch (err) {
        console.error(
          `[playwright] restart attempt ${this.restartAttempt} failed:`,
          err instanceof Error ? err.message : err
        );
        // If shutdown happened during the failed attempt, stop chaining.
        if (this.closingIntentionally) return;
        // Chain into the next attempt — the loop continues until success
        // or MAX_RESTART_ATTEMPTS.
        this.restarting = null;
        await this.scheduleRestart(`retry-after-failure (${reason})`);
      } finally {
        this.restarting = null;
      }
    })();
    return await this.restarting;
  }

  async execute(
    action: BridgeAction,
    params: Record<string, unknown>
  ): Promise<BridgeResponse> {
    const id = crypto.randomUUID();
    // If the supervisor is mid-restart (context is null/closing during
    // backoff + relaunch), wait for it to finish before dispatching.
    // Otherwise the request would fail immediately against a dead context
    // instead of seeing the daemon as briefly slow. We never throw out of
    // this wait — if the restart itself failed, dispatch will surface a
    // more specific error.
    if (this.restarting) {
      try {
        await this.restarting;
      } catch {
        // swallow — supervisor logged it; dispatch will report a concrete
        // error if the context still is not usable.
      }
    }
    // Determine the queue scope AND pin the page identity at enqueue time.
    // The pinned UUID rides into dispatch via __pageUuid so that even if the
    // positional tabId shifts during the queue wait (because another action
    // closed an earlier tab), the action still resolves to the same Page.
    const { scope, pinnedParams } = this.scopeFor(action, params);
    try {
      const data = await this.queue.enqueue(scope, () =>
        this.dispatch(action, pinnedParams)
      );
      // Any successful action may have mutated cookies / localStorage /
      // navigation history. Mark the snapshot dirty so the next refresh
      // tick re-reads storageState. False positives are cheap; misses
      // would lose state on a crash restart.
      this.snapshot.markDirty();
      return { id, success: true, data };
    } catch (error) {
      return {
        id,
        success: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  // Classify each action into a serialization scope AND, when the action
  // targets a specific tab, pin that target by its stable UUID. Three cases:
  //   - GLOBAL: structural / multi-tab operations and active-tab implicit
  //     actions — they cannot pin a single page without racing.
  //   - per-page UUID: explicit tabId resolves to a Page right now; we
  //     attach its UUID to params so dispatch reaches the same Page even
  //     after positional shifts.
  //   - GLOBAL fallback: explicit tabId that no longer resolves. The
  //     dispatcher surfaces the real error under the global lock.
  private scopeFor(
    action: BridgeAction,
    params: Record<string, unknown>
  ): { scope: Scope; pinnedParams: Record<string, unknown> } {
    // Always-global: any structural / cross-tab action serializes against
    // the world. But if it names a specific tabId, we still pin the target
    // Page now so positional shifts during the queue wait do not redirect
    // the close/activate to the wrong tab.
    switch (action) {
      case "tabs.open":
      case "tabs.list":
      case "secrets.put":
      case "secrets.delete":
      case "secrets.list":
        return { scope: GLOBAL, pinnedParams: params };
      case "tabs.close":
      case "tabs.activate":
        return {
          scope: GLOBAL,
          pinnedParams: this.pinByTabId(params),
        };
      default:
        break;
    }
    if (typeof params.tabId !== "number") {
      return { scope: GLOBAL, pinnedParams: params };
    }
    try {
      const page = this.pageFromTabId(params.tabId);
      const uuid = this.pageUuids.get(page);
      if (!uuid) {
        // Page exists but is not tracked — treat as already-gone.
        return {
          scope: GLOBAL,
          pinnedParams: { ...params, __pageUuid: UNRESOLVED_TAB },
        };
      }
      return {
        scope: uuid,
        pinnedParams: { ...params, __pageUuid: uuid },
      };
    } catch {
      // Tab does not currently resolve. Mark it so dispatch surfaces a
      // clean "tab closed" error rather than letting the positional tabId
      // get re-resolved into a freshly-opened page after a global action.
      return {
        scope: GLOBAL,
        pinnedParams: { ...params, __pageUuid: UNRESOLVED_TAB },
      };
    }
  }

  // Pin a Page identity onto params without changing the queue scope.
  // Used by structural actions (tabs.close/activate) that must serialize
  // globally but still need to target the original tab even after the
  // positional index shifts.
  private pinByTabId(
    params: Record<string, unknown>
  ): Record<string, unknown> {
    if (typeof params.tabId !== "number") return params;
    try {
      const page = this.pageFromTabId(params.tabId);
      const uuid = this.pageUuids.get(page);
      return uuid
        ? { ...params, __pageUuid: uuid }
        : { ...params, __pageUuid: UNRESOLVED_TAB };
    } catch {
      return { ...params, __pageUuid: UNRESOLVED_TAB };
    }
  }

  private async dispatch(
    action: BridgeAction,
    params: Record<string, unknown>
  ): Promise<unknown> {
    switch (action) {
      case "tabs.list":
        return this.listTabs();
      case "tabs.open":
        return this.openTab(String(params.url ?? "about:blank"));
      case "tabs.close":
        return this.closeTab(this.requirePage(params));
      case "tabs.navigate":
        return this.navigate(this.getPage(params), String(params.url ?? ""));
      case "tabs.activate":
        return this.activateTab(this.requirePage(params));
      case "tabs.goBack":
        return this.goBack(this.getPage(params));
      case "tabs.goForward":
        return this.goForward(this.getPage(params));
      case "tabs.reload":
        return this.reload(this.getPage(params));
      case "dom.getHtml":
        return this.getHtml(params);
      case "dom.getText":
        return this.getText(params);
      case "dom.contentSummary":
        return this.getContentSummary(params);
      case "dom.querySelector":
        return this.querySelector(params);
      case "dom.formValues":
        return this.getFormValues(params);
      case "dom.accessibilityTree":
        return this.getAccessibilityTree(params);
      case "interaction.click":
        return this.click(params);
      case "interaction.type":
        return this.typeText(params);
      case "interaction.typeSecret":
        return this.typeSecret(params);
      case "interaction.scroll":
        return this.scroll(params);
      case "interaction.pressKey":
        return this.pressKey(params);
      case "interaction.hover":
        return this.hover(params);
      case "interaction.mouseMove":
        return this.mouseMove(params);
      case "interaction.selectOption":
        return this.selectOption(params);
      case "interaction.check":
        return this.check(params);
      case "interaction.clickAnnotation":
        return this.clickAnnotation(params);
      case "interaction.typeAnnotation":
        return this.typeAnnotation(params);
      case "capture.screenshot":
        return this.captureScreenshot(params);
      case "capture.computedStyles":
        return this.getComputedStyles(params);
      case "capture.elementRect":
        return this.getElementRect(params);
      case "capture.metrics":
        return this.getPageMetrics(params);
      case "capture.annotate":
        return this.annotatePage(params);
      case "capture.clearAnnotations":
        return this.clearAnnotations(params);
      case "capture.highlight":
        return this.highlight(params);
      case "execution.executeJs":
        return this.executeJs(params);
      case "wait.selector":
        return this.waitForSelector(params);
      case "wait.navigation":
        return this.waitForNavigation(params);
      case "wait.networkIdle":
        return this.waitForNetworkIdle(params);
      case "wait.url":
        return this.waitForUrl(params);
      case "cookies.get":
        return this.getCookies(params);
      case "cookies.set":
        return this.setCookie(params);
      case "cookies.delete":
        return this.deleteCookie(params);
      case "storage.get":
        return this.getStorage(params);
      case "storage.set":
        return this.setStorage(params);
      case "storage.clear":
        return this.clearStorage(params);
      case "dialog.setBehavior":
        return this.setDialogBehavior(params);
      case "dialog.getLast":
        return this.getLastDialog(params);
      case "monitor.consoleLogs":
        return this.getConsoleLogs(params);
      case "monitor.pageErrors":
        return this.getPageErrors(params);
      case "monitor.networkLogs":
        return this.getNetworkLogs(params);
      case "secrets.put":
        return this.secrets.put(
          String(params.value ?? ""),
          typeof params.label === "string" ? params.label : undefined
        );
      case "secrets.delete": {
        const id = String(params.id ?? params.secretId ?? "");
        if (!id) throw new Error("id is required");
        await this.secrets.delete(id);
        return null;
      }
      case "secrets.list":
        return this.secrets.list();
      case "daemon.ping":
      case "daemon.shutdown":
      case "daemon.health":
        // Daemon control actions are intercepted in the socket server, not
        // routed through the driver. Hitting here means a misrouted request.
        throw new Error(`${action} is handled by the daemon, not the driver`);
      default:
        throw new Error(`Unsupported action: ${action satisfies never}`);
    }
  }

  private resolveBrowserType(): BrowserType {
    switch (this.options.browserName) {
      case "chromium":
        return chromium;
      case "firefox":
        return firefox;
      case "webkit":
        return webkit;
      default:
        throw new Error(
          `Unsupported BROWSER_NAME=${this.options.browserName}. Use chromium, firefox, or webkit.`
        );
    }
  }

  private resolveExecutablePath(): string | undefined {
    if (this.options.executablePath) {
      return this.options.executablePath;
    }

    const candidates =
      this.options.browserName === "firefox"
        ? ["/usr/bin/firefox"]
        : this.options.browserName === "webkit"
        ? []
        : [
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
          ];

    return candidates.find((candidate) => existsSync(candidate));
  }

  private attachPage(page: Page): void {
    if (this.pageStates.has(page)) {
      return;
    }

    // Assign a stable UUID per Page object the first time we see it. This is
    // what the ActionQueue uses to keep concurrent requests against the same
    // physical tab in order even as positional tabIds shift around close/popup.
    if (!this.pageUuids.has(page)) {
      this.pageUuids.set(page, randomBytes(8).toString("hex"));
    }
    // Snapshot tracking — URL is updated continuously via framenavigated
    // so the supervisor always has the latest known URL even if the
    // context dies before we can query it.
    const uuid = this.pageUuids.get(page);
    if (uuid) this.snapshot.trackPage(page, uuid);

    const state: PageState = {
      consoleLogs: [],
      pageErrors: [],
      networkLogs: [],
      requestIndex: new Map(),
      dialogBehavior: { action: "dismiss" },
      lastDialog: null,
    };
    this.pageStates.set(page, state);

    page.on("console", (message) => this.recordConsole(page, message));
    page.on("pageerror", (error) => {
      state.pageErrors.push({
        message: error.message,
        stack: error.stack ?? null,
        time: Date.now(),
      });
      trimEntries(state.pageErrors);
    });
    page.on("request", (request) => this.recordRequest(state, request));
    page.on("response", (response) => {
      void this.recordResponse(state, response);
    });
    page.on("requestfailed", (request) =>
      this.recordRequestFailed(state, request)
    );
    page.on("dialog", (dialog) => void this.handleDialog(page, dialog));
    page.on("close", () => this.pageStates.delete(page));
  }

  private recordRequest(state: PageState, request: Request): void {
    const entry: NetworkEntry = {
      id: crypto.randomUUID(),
      url: request.url(),
      method: request.method(),
      resourceType: request.resourceType(),
      status: null,
      statusText: null,
      requestHeaders: sanitizeHeaders(request.headers()),
      responseHeaders: {},
      requestBody: clipText(request.postData() ?? null, MAX_BODY_BYTES),
      responseBody: null,
      responseBodyTruncated: false,
      responseBodySize: null,
      startTime: Date.now(),
      endTime: null,
      durationMs: null,
      fromCache: false,
      failed: false,
      failureText: null,
    };
    state.requestIndex.set(request, entry);
    state.networkLogs.push(entry);
    trimNetwork(state.networkLogs, state.requestIndex);
  }

  private async recordResponse(
    state: PageState,
    response: Response
  ): Promise<void> {
    const entry = state.requestIndex.get(response.request());
    if (!entry) {
      return;
    }
    entry.status = response.status();
    entry.statusText = response.statusText();
    entry.responseHeaders = sanitizeHeaders(await response.headers());
    entry.fromCache = response.fromServiceWorker();
    entry.endTime = Date.now();
    entry.durationMs = entry.endTime - entry.startTime;
    const contentType = entry.responseHeaders["content-type"] ?? "";
    if (!TEXT_CONTENT_TYPES.test(contentType)) {
      return;
    }
    const declared = Number(entry.responseHeaders["content-length"] ?? "");
    if (Number.isFinite(declared) && declared > MAX_BODY_BYTES) {
      entry.responseBodySize = declared;
      entry.responseBodyTruncated = true;
      return;
    }
    try {
      const body = await response.body();
      entry.responseBodySize = body.byteLength;
      if (body.byteLength > MAX_BODY_BYTES) {
        entry.responseBody = body.slice(0, MAX_BODY_BYTES).toString("utf8");
        entry.responseBodyTruncated = true;
      } else {
        entry.responseBody = body.toString("utf8");
      }
    } catch {
      // body unavailable (e.g. redirect without body) — ignore
    }
  }

  private recordRequestFailed(state: PageState, request: Request): void {
    const entry = state.requestIndex.get(request);
    if (!entry) {
      return;
    }
    entry.failed = true;
    entry.failureText = request.failure()?.errorText ?? "request failed";
    entry.endTime = Date.now();
    entry.durationMs = entry.endTime - entry.startTime;
  }

  private recordConsole(page: Page, message: ConsoleMessage): void {
    const state = this.pageStates.get(page);
    if (!state) {
      return;
    }
    state.consoleLogs.push({
      type: message.type(),
      text: message.text(),
      location: message.location(),
      time: Date.now(),
    });
    trimEntries(state.consoleLogs);
  }

  private async handleDialog(page: Page, dialog: Dialog): Promise<void> {
    const state = this.getPageState(page);
    state.lastDialog = {
      type: dialog.type(),
      message: dialog.message(),
      defaultValue: dialog.defaultValue(),
      time: Date.now(),
    };
    const behavior = state.dialogBehavior;
    if (behavior.action === "accept") {
      await dialog.accept(behavior.text ?? "");
    } else {
      await dialog.dismiss();
    }
  }

  private async listTabs(): Promise<Array<Record<string, unknown>>> {
    return this.pages().map((page, index) => ({
      tabId: index + 1,
      title: page.url() === "about:blank" ? "about:blank" : page.url(),
      url: page.url(),
      active: index + 1 === this.resolveActiveTabId(),
    }));
  }

  private async openTab(url: string): Promise<Record<string, unknown>> {
    const page = await this.requireContext().newPage();
    this.attachPage(page);
    this.activeTabId = this.tabIdForPage(page);
    if (url && url !== "about:blank") {
      await this.goto(page, url);
    }
    return {
      tabId: this.tabIdForPage(page),
      url: page.url(),
      title: await page.title().catch(() => page.url()),
    };
  }

  private async closeTab(page: Page): Promise<null> {
    // Drop from snapshot BEFORE the close — otherwise the keeper would
    // see the page emit "close" and keep its last URL for a phantom
    // restore on a future crash.
    const uuid = this.pageUuids.get(page);
    if (uuid) this.snapshot.dropPage(uuid);
    await page.close();
    this.activeTabId = this.pages()[0] ? 1 : null;
    return null;
  }

  private async activateTab(page: Page): Promise<null> {
    this.activeTabId = this.tabIdForPage(page);
    await page.bringToFront().catch(() => undefined);
    return null;
  }

  private async navigate(
    page: Page,
    url: string
  ): Promise<Record<string, unknown>> {
    await this.goto(page, url);
    return {
      tabId: this.tabIdForPage(page),
      url: page.url(),
      title: await page.title().catch(() => page.url()),
    };
  }

  private async goBack(page: Page): Promise<null> {
    await page.goBack({ waitUntil: "domcontentloaded" });
    return null;
  }

  private async goForward(page: Page): Promise<null> {
    await page.goForward({ waitUntil: "domcontentloaded" });
    return null;
  }

  private async reload(page: Page): Promise<null> {
    await page.reload({ waitUntil: "domcontentloaded" });
    return null;
  }

  private async getHtml(params: Record<string, unknown>): Promise<string> {
    const page = this.getPage(params);
    const selector =
      typeof params.selector === "string" ? params.selector : null;
    const outer = params.outer !== false;
    const clean = params.clean !== false;
    if (!selector) {
      const html = await page.content();
      return clean ? cleanupHtml(html) : html;
    }
    const locator = page.locator(selector).first();
    const html = outer
      ? await locator.evaluate((el) => el.outerHTML)
      : await locator.evaluate((el) => el.innerHTML);
    return clean ? cleanupHtml(html) : html;
  }

  private async getText(params: Record<string, unknown>): Promise<string> {
    const page = this.getPage(params);
    const selector =
      typeof params.selector === "string" ? params.selector : null;
    const raw = params.raw === true;
    const mainContent = params.mainContent !== false;
    if (selector) {
      const locator = page.locator(selector).first();
      return raw
        ? await locator.evaluate((el) => el.textContent ?? "")
        : normalizeText(await locator.innerText());
    }
    if (raw) {
      return normalizeText(
        await page.evaluate(() => document.body.textContent ?? "")
      );
    }
    if (mainContent) {
      return normalizeText(
        await page.evaluate(() => {
          const main =
            document.querySelector("main, article, [role='main']") ??
            document.body;
          return (main as HTMLElement).innerText ?? main.textContent ?? "";
        })
      );
    }
    return normalizeText(await page.locator("body").innerText());
  }

  private async querySelector(
    params: Record<string, unknown>
  ): Promise<Array<Record<string, unknown>>> {
    const page = this.getPage(params);
    const selector = String(params.selector ?? "");
    const limit = Number(params.limit ?? 20);
    const visibleOnly = params.visibleOnly === true;
    return await page.locator(selector).evaluateAll(
      (elements, options) =>
        elements
          .filter((el) => {
            if (!options.visibleOnly) return true;
            const rect = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            return (
              rect.width > 0 &&
              rect.height > 0 &&
              style.visibility !== "hidden" &&
              style.display !== "none"
            );
          })
          .slice(0, options.limit)
          .map((el, index) => {
            const rect = el.getBoundingClientRect();
            return {
              index,
              tag: el.tagName.toLowerCase(),
              id: el.id || null,
              classes: [...el.classList],
              text: ((el as HTMLElement).innerText || el.textContent || "")
                .trim()
                .slice(0, 200),
              attributes: Object.fromEntries(
                [...el.attributes].map((attr) => [attr.name, attr.value])
              ),
              rect: {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
              },
            };
          }),
      { limit, visibleOnly }
    );
  }

  private async getContentSummary(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const page = this.getPage(params);
    const selector =
      typeof params.selector === "string" ? params.selector : null;
    const maxHeadings = Number(params.maxHeadings ?? 20);
    const maxLinks = Number(params.maxLinks ?? 20);
    const maxTextLength = Number(params.maxTextLength ?? 4000);

    return await page.evaluate(
      ({ selector, maxHeadings, maxLinks, maxTextLength }) => {
        const noiseSelectors = [
          "script",
          "style",
          "svg",
          "noscript",
          "nav",
          "footer",
          "header",
          "aside",
          "[role='navigation']",
          "[aria-hidden='true']",
          ".sr-only",
          ".visually-hidden",
          ".hidden",
          "#cookie-banner",
          "#cookies",
          ".cookie-banner",
          ".cookie-notice",
          ".advertisement",
          ".ads",
        ];

        const pickRoot = () => {
          if (selector) {
            return document.querySelector(selector);
          }
          return (
            document.querySelector("main") ||
            document.querySelector("article") ||
            document.querySelector("[role='main']") ||
            document.body
          );
        };

        const root = pickRoot();
        if (!root) {
          throw new Error("Summary target not found");
        }

        const clone = root.cloneNode(true);
        if (!(clone instanceof HTMLElement)) {
          throw new Error("Summary target is not an element");
        }

        clone
          .querySelectorAll(noiseSelectors.join(","))
          .forEach((el) => el.remove());

        clone.querySelectorAll("*").forEach((el) => {
          const style = window.getComputedStyle(el);
          if (style.display === "none" || style.visibility === "hidden") {
            el.remove();
          }
        });

        const cleanText = (text: string | null | undefined) =>
          (text || "")
            .replace(/\u00a0/g, " ")
            .replace(/[ \t]+\n/g, "\n")
            .replace(/\n{3,}/g, "\n\n")
            .replace(/[ \t]{2,}/g, " ")
            .trim();

        const headings = [...clone.querySelectorAll("h1,h2,h3,h4,h5,h6")]
          .slice(0, maxHeadings)
          .map((el) => ({
            level: el.tagName.toLowerCase(),
            text: cleanText(el.textContent).slice(0, 200),
          }))
          .filter((item) => item.text);

        const links = [...clone.querySelectorAll("a[href]")]
          .slice(0, maxLinks)
          .map((el) => ({
            text: cleanText(el.textContent).slice(0, 160),
            href: el.getAttribute("href"),
          }))
          .filter((item) => item.text || item.href);

        const forms = [...clone.querySelectorAll("form")]
          .slice(0, 10)
          .map((form, index) => ({
            index,
            fields: [...form.querySelectorAll("input,textarea,select,button")]
              .slice(0, 20)
              .map((el) => ({
                tag: el.tagName.toLowerCase(),
                type: "type" in el ? el.type || null : null,
                name: el.getAttribute("name"),
                id: el.getAttribute("id"),
                placeholder: el.getAttribute("placeholder"),
                label:
                  el.getAttribute("aria-label") ||
                  (el instanceof HTMLElement
                    ? cleanText(el.innerText).slice(0, 80)
                    : ""),
              })),
          }));

        const text = cleanText(
          clone.innerText || clone.textContent || ""
        ).slice(0, maxTextLength);

        return {
          url: location.href,
          title: document.title,
          selector: selector ?? null,
          headings,
          links,
          forms,
          text,
        };
      },
      { selector, maxHeadings, maxLinks, maxTextLength }
    );
  }

  private async getFormValues(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const page = this.getPage(params);
    const selector = String(params.selector ?? "");
    return await page
      .locator(selector)
      .first()
      .evaluate((form) => {
        if (!(form instanceof HTMLFormElement)) {
          throw new Error("Selector does not point to a form");
        }
        const data: Record<string, unknown> = {};
        for (const el of [...form.elements]) {
          if (
            !(el instanceof HTMLElement) ||
            !("name" in el) ||
            typeof el.name !== "string" ||
            !el.name
          ) {
            continue;
          }
          if (
            el instanceof HTMLInputElement &&
            (el.type === "checkbox" || el.type === "radio")
          ) {
            data[el.name] = el.checked;
          } else if ("value" in el) {
            data[el.name] = (
              el as HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement
            ).value;
          }
        }
        return data;
      });
  }

  private async getAccessibilityTree(
    params: Record<string, unknown>
  ): Promise<string> {
    const page = this.getPage(params);
    const maxElements = Number(params.maxElements ?? 500);
    const entries = await page
      .locator("a,button,input,select,textarea,[role],[tabindex]")
      .evaluateAll(
        (elements, limit) =>
          elements.slice(0, limit).map((el, index) => ({
            ref: index + 1,
            tag: el.tagName.toLowerCase(),
            role: el.getAttribute("role") || null,
            name: (
              el.getAttribute("aria-label") ||
              (el as HTMLElement).innerText ||
              (el as HTMLInputElement).value ||
              el.getAttribute("name") ||
              ""
            )
              .trim()
              .slice(0, 160),
            disabled:
              el instanceof HTMLButtonElement ||
              el instanceof HTMLInputElement ||
              el instanceof HTMLSelectElement ||
              el instanceof HTMLTextAreaElement
                ? el.disabled
                : el.getAttribute("aria-disabled") === "true",
          })),
        maxElements
      );
    return entries
      .map(
        (entry) =>
          `@${entry.ref} <${entry.tag}> role=${entry.role ?? "none"} name="${
            entry.name
          }" disabled=${entry.disabled}`
      )
      .join("\n");
  }

  private async click(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    await page
      .locator(String(params.selector ?? ""))
      .first()
      .click();
    return null;
  }

  private async typeText(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const locator = page.locator(String(params.selector ?? "")).first();
    if (params.clear !== false) {
      await locator.clear();
    }
    await locator.fill(String(params.text ?? ""));
    return null;
  }

  private async typeSecret(params: Record<string, unknown>): Promise<null> {
    const secret = await this.secrets.get(String(params.secretId ?? ""));
    const page = this.getPage(params);
    const locator = page.locator(String(params.selector ?? "")).first();
    if (params.clear !== false) {
      await locator.clear();
    }
    await locator.fill(secret);
    return null;
  }

  private async scroll(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    if (typeof params.selector === "string") {
      await page.locator(params.selector).first().scrollIntoViewIfNeeded();
      return null;
    }
    const x = Number(params.x ?? 0);
    const y = Number(params.y ?? 0);
    await page.evaluate(([dx, dy]) => window.scrollBy(dx, dy), [x, y] as const);
    return null;
  }

  private async pressKey(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    if (typeof params.selector === "string") {
      await page.locator(params.selector).first().focus();
    }
    await page.keyboard.press(String(params.key ?? ""));
    return null;
  }

  private async hover(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const selector = String(params.selector ?? "");
    if (!selector) {
      throw new Error("selector is required");
    }
    const position =
      typeof params.x === "number" && typeof params.y === "number"
        ? { x: Number(params.x), y: Number(params.y) }
        : undefined;
    await page
      .locator(selector)
      .first()
      .hover(position ? { position } : undefined);
    return null;
  }

  private async mouseMove(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const x = Number(params.x ?? 0);
    const y = Number(params.y ?? 0);
    const steps = typeof params.steps === "number" ? Number(params.steps) : 1;
    await page.mouse.move(x, y, { steps });
    return null;
  }

  private async selectOption(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const locator = page.locator(String(params.selector ?? "")).first();
    const option =
      typeof params.value === "string"
        ? { value: params.value }
        : typeof params.label === "string"
        ? { label: params.label }
        : typeof params.index === "number"
        ? { index: params.index }
        : undefined;
    if (!option) {
      throw new Error("Provide value, label, or index");
    }
    await locator.selectOption(option);
    return null;
  }

  private async check(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const locator = page.locator(String(params.selector ?? "")).first();
    if (params.checked === false) {
      await locator.uncheck();
    } else {
      await locator.check();
    }
    return null;
  }

  private async clickAnnotation(
    params: Record<string, unknown>
  ): Promise<null> {
    const page = this.getPage(params);
    const ref = Number(params.ref);
    await page.locator(`[data-ai-browser-ref="${ref}"]`).first().click();
    return null;
  }

  private async typeAnnotation(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const ref = Number(params.ref);
    const locator = page.locator(`[data-ai-browser-ref="${ref}"]`).first();
    if (params.clear !== false) {
      await locator.clear();
    }
    await locator.fill(String(params.text ?? ""));
    return null;
  }

  private async captureScreenshot(
    params: Record<string, unknown>
  ): Promise<string> {
    const page = this.getPage(params);
    const buffer = await page.screenshot({ type: "png" });
    return `data:image/png;base64,${buffer.toString("base64")}`;
  }

  private async getComputedStyles(
    params: Record<string, unknown>
  ): Promise<Record<string, string>> {
    const page = this.getPage(params);
    const selector = String(params.selector ?? "");
    const properties = Array.isArray(params.properties)
      ? params.properties.map(String)
      : null;
    return await page
      .locator(selector)
      .first()
      .evaluate((el, props) => {
        const style = getComputedStyle(el);
        const keys = props ?? Array.from(style);
        return Object.fromEntries(
          keys.map((key) => [key, style.getPropertyValue(key)])
        );
      }, properties);
  }

  private async getElementRect(
    params: Record<string, unknown>
  ): Promise<Record<string, number>> {
    const page = this.getPage(params);
    const box = await page
      .locator(String(params.selector ?? ""))
      .first()
      .boundingBox();
    if (!box) {
      throw new Error("Element is not visible");
    }
    return {
      ...box,
      devicePixelRatio: await page.evaluate(() => window.devicePixelRatio || 1),
    };
  }

  private async getPageMetrics(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const page = this.getPage(params);
    return await page.evaluate(() => {
      const nav = performance.getEntriesByType("navigation")[0] as
        | PerformanceNavigationTiming
        | undefined;
      return {
        url: location.href,
        title: document.title,
        readyState: document.readyState,
        domNodes: document.getElementsByTagName("*").length,
        resources: performance.getEntriesByType("resource").length,
        navigation: nav
          ? {
              type: nav.type,
              domContentLoaded: nav.domContentLoadedEventEnd,
              loadEventEnd: nav.loadEventEnd,
            }
          : null,
      };
    });
  }

  private async annotatePage(
    params: Record<string, unknown>
  ): Promise<{ count: number }> {
    const page = this.getPage(params);
    return await page.evaluate(() => {
      const existing = document.getElementById("__ai_browser_overlay_root__");
      existing?.remove();

      const root = document.createElement("div");
      root.id = "__ai_browser_overlay_root__";
      root.style.position = "absolute";
      root.style.inset = "0";
      root.style.pointerEvents = "none";
      root.style.zIndex = "2147483647";
      document.documentElement.appendChild(root);

      const interactive = [
        ...document.querySelectorAll(
          "a,button,input,select,textarea,[role='button'],[role='link'],[tabindex]"
        ),
      ].filter((el) => {
        const rect = el.getBoundingClientRect();
        const style = getComputedStyle(el);
        return (
          rect.width > 0 &&
          rect.height > 0 &&
          style.display !== "none" &&
          style.visibility !== "hidden"
        );
      });

      interactive.forEach((el, index) => {
        const ref = String(index + 1);
        el.setAttribute("data-ai-browser-ref", ref);
        const rect = el.getBoundingClientRect();
        const badge = document.createElement("div");
        badge.textContent = ref;
        badge.style.position = "absolute";
        badge.style.left = `${window.scrollX + rect.left}px`;
        badge.style.top = `${window.scrollY + rect.top}px`;
        badge.style.background = "#d92d20";
        badge.style.color = "#fff";
        badge.style.font = "12px/1 monospace";
        badge.style.padding = "2px 4px";
        badge.style.borderRadius = "4px";
        badge.style.pointerEvents = "none";
        root.appendChild(badge);
      });

      return { count: interactive.length };
    });
  }

  private async clearAnnotations(
    params: Record<string, unknown>
  ): Promise<null> {
    const page = this.getPage(params);
    await page.evaluate(() => {
      document
        .querySelectorAll("[data-ai-browser-ref]")
        .forEach((el) => el.removeAttribute("data-ai-browser-ref"));
      document.getElementById("__ai_browser_overlay_root__")?.remove();
    });
    return null;
  }

  private async highlight(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const selector = String(params.selector ?? "");
    const color = String(params.color ?? "rgba(229, 62, 62, 0.3)");
    const duration = Number(params.duration ?? 3000);
    await page
      .locator(selector)
      .first()
      .evaluate(
        (el, data) => {
          const rect = el.getBoundingClientRect();
          const overlay = document.createElement("div");
          overlay.id = "__ai_browser_highlight__";
          overlay.style.position = "absolute";
          overlay.style.left = `${window.scrollX + rect.left}px`;
          overlay.style.top = `${window.scrollY + rect.top}px`;
          overlay.style.width = `${rect.width}px`;
          overlay.style.height = `${rect.height}px`;
          overlay.style.background = data.color;
          overlay.style.outline = "2px solid #d92d20";
          overlay.style.pointerEvents = "none";
          overlay.style.zIndex = "2147483647";
          document.body.appendChild(overlay);
          window.setTimeout(() => overlay.remove(), data.duration);
        },
        { color, duration }
      );
    return null;
  }

  private async executeJs(params: Record<string, unknown>): Promise<unknown> {
    const page = this.getPage(params);
    const code = String(params.code ?? "");
    return await page.evaluate((source) => {
      return globalThis.eval(source);
    }, code);
  }

  private async waitForSelector(
    params: Record<string, unknown>
  ): Promise<null> {
    const page = this.getPage(params);
    await page
      .locator(String(params.selector ?? ""))
      .first()
      .waitFor({
        state: params.visible === true ? "visible" : "attached",
        timeout: Number(params.timeout ?? 10000),
      });
    return null;
  }

  private async waitForNavigation(
    params: Record<string, unknown>
  ): Promise<null> {
    const page = this.getPage(params);
    await page.waitForLoadState("load", {
      timeout: Number(params.timeout ?? 30000),
    });
    return null;
  }

  private async waitForNetworkIdle(
    params: Record<string, unknown>
  ): Promise<null> {
    const page = this.getPage(params);
    await page.waitForLoadState("networkidle", {
      timeout: Number(params.timeout ?? 10000),
    });
    return null;
  }

  private async waitForUrl(
    params: Record<string, unknown>
  ): Promise<{ url: string }> {
    const page = this.getPage(params);
    const pattern = String(params.pattern ?? "");
    const patternType = (params.patternType as UrlPatternType) ?? "exact";
    const timeout = Number(params.timeout ?? 30000);
    const match = compileUrlMatcher(pattern, patternType);
    await page.waitForURL((url) => match(url.toString()), { timeout });
    return { url: page.url() };
  }

  private async getCookies(
    params: Record<string, unknown>
  ): Promise<Array<Record<string, unknown>>> {
    const url =
      typeof params.url === "string" && params.url
        ? this.normalizeUrl(params.url)
        : this.getPage(params).url();
    return (await this.requireContext().cookies([url])).map((cookie) => ({
      name: cookie.name,
      value: cookie.value,
      domain: cookie.domain,
      path: cookie.path,
      expires: cookie.expires,
      httpOnly: cookie.httpOnly,
      secure: cookie.secure,
      sameSite: cookie.sameSite,
    }));
  }

  private async setCookie(params: Record<string, unknown>): Promise<null> {
    // Playwright's addCookies rejects entries with BOTH url and domain set.
    // If the caller provided a domain explicitly, honor that. Otherwise
    // derive everything from the url (which is the common case).
    const hasDomain = typeof params.domain === "string" && params.domain;
    const rawUrl = String(params.url ?? "");
    if (!hasDomain && !rawUrl) {
      throw new Error("cookies.set requires either url or domain");
    }
    const baseCookie: Record<string, unknown> = {
      name: String(params.name ?? ""),
      value: String(params.value ?? ""),
      secure: params.secure === true,
      httpOnly: params.httpOnly === true,
    };
    if (typeof params.expirationDate === "number") {
      baseCookie.expires = params.expirationDate;
    }
    if (hasDomain) {
      // domain mode: domain + path required, url disallowed
      baseCookie.domain = params.domain as string;
      baseCookie.path = typeof params.path === "string" ? params.path : "/";
    } else {
      // url mode: domain/path derived from url, must not be set explicitly
      baseCookie.url = this.normalizeUrl(rawUrl);
      if (typeof params.path === "string") {
        // Caller gave an explicit path — drop the url and use domain+path
        baseCookie.domain = new URL(this.normalizeUrl(rawUrl)).hostname;
        baseCookie.path = params.path;
        delete (baseCookie as { url?: unknown }).url;
      }
    }
    await this.requireContext().addCookies([
      baseCookie as Parameters<BrowserContext["addCookies"]>[0][number],
    ]);
    return null;
  }

  private async deleteCookie(params: Record<string, unknown>): Promise<null> {
    const url = this.normalizeUrl(String(params.url ?? ""));
    const name = String(params.name ?? "");
    const context = this.requireContext();
    const cookies = await context.cookies([url]);
    await context.clearCookies();
    const remaining = cookies.filter((cookie) => cookie.name !== name);
    if (remaining.length > 0) {
      await context.addCookies(remaining);
    }
    return null;
  }

  private async getStorage(
    params: Record<string, unknown>
  ): Promise<Record<string, string> | string | null> {
    const page = this.getPage(params);
    const type = params.type === "session" ? "sessionStorage" : "localStorage";
    if (typeof params.key === "string") {
      return await page.evaluate(
        ([storageType, key]) =>
          window[storageType as "localStorage" | "sessionStorage"].getItem(key),
        [type, params.key] as const
      );
    }
    return await page.evaluate((storageType) => {
      const store = window[storageType as "localStorage" | "sessionStorage"];
      return Object.fromEntries(
        Object.keys(store).map((key) => [key, store.getItem(key) ?? ""])
      );
    }, type);
  }

  private async setStorage(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const type = params.type === "session" ? "sessionStorage" : "localStorage";
    await page.evaluate(
      ([storageType, key, value]) => {
        window[storageType as "localStorage" | "sessionStorage"].setItem(
          key,
          value
        );
      },
      [type, String(params.key ?? ""), String(params.value ?? "")] as const
    );
    return null;
  }

  private async clearStorage(params: Record<string, unknown>): Promise<null> {
    const page = this.getPage(params);
    const type = params.type === "session" ? "sessionStorage" : "localStorage";
    await page.evaluate((storageType) => {
      window[storageType as "localStorage" | "sessionStorage"].clear();
    }, type);
    return null;
  }

  private async setDialogBehavior(
    params: Record<string, unknown>
  ): Promise<null> {
    const page = this.getPage(params);
    const state = this.getPageState(page);
    state.dialogBehavior = {
      action: params.action === "accept" ? "accept" : "dismiss",
      text: typeof params.text === "string" ? params.text : undefined,
    };
    return null;
  }

  private async getLastDialog(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown> | null> {
    return this.getPageState(this.getPage(params)).lastDialog;
  }

  private async getConsoleLogs(
    params: Record<string, unknown>
  ): Promise<Array<Record<string, unknown>>> {
    const entries = this.getPageState(this.getPage(params)).consoleLogs;
    const level = typeof params.level === "string" ? params.level : "all";
    const limit = Number(params.limit ?? 100);
    return entries
      .filter((entry) => level === "all" || entry.type === level)
      .slice(-limit);
  }

  private async getPageErrors(
    params: Record<string, unknown>
  ): Promise<Array<Record<string, unknown>>> {
    const limit = Number(params.limit ?? 50);
    return this.getPageState(this.getPage(params)).pageErrors.slice(-limit);
  }

  private async getNetworkLogs(
    params: Record<string, unknown>
  ): Promise<NetworkEntry[]> {
    const state = this.getPageState(this.getPage(params));
    const limit = Number(params.limit ?? 100);
    const method =
      typeof params.method === "string" ? params.method.toUpperCase() : null;
    const statusParam = params.status;
    const urlPattern =
      typeof params.urlPattern === "string" && params.urlPattern.length > 0
        ? safeRegex(params.urlPattern)
        : null;
    const includeBody = params.includeBody === true;

    const filtered = state.networkLogs.filter((entry) => {
      if (method && entry.method !== method) return false;
      if (urlPattern && !urlPattern.test(entry.url)) return false;
      if (typeof statusParam === "number") {
        if (entry.status !== statusParam) return false;
      } else if (typeof statusParam === "string") {
        const range = statusParam.match(/^(\d)xx$/i);
        if (range) {
          if (entry.status === null) return false;
          const bucket = Math.floor(entry.status / 100);
          if (bucket !== Number(range[1])) return false;
        }
      }
      return true;
    });

    const sliced = filtered.slice(-limit);
    if (includeBody) {
      return sliced;
    }
    return sliced.map((entry) => ({
      ...entry,
      requestBody: null,
      responseBody: null,
    }));
  }

  private getPage(params: Record<string, unknown>): Page {
    // __pageUuid wins over tabId so a queued action lands on the same Page
    // even if the positional index shifted while it was waiting.
    if (typeof params.__pageUuid === "string") {
      const page = this.pageFromUuid(params.__pageUuid);
      this.activeTabId = this.tabIdForPage(page);
      return page;
    }
    const page = this.pageFromTabId(
      typeof params.tabId === "number"
        ? params.tabId
        : this.resolveActiveTabId()
    );
    this.activeTabId = this.tabIdForPage(page);
    return page;
  }

  private requirePage(params: Record<string, unknown>): Page {
    if (typeof params.__pageUuid === "string") {
      return this.pageFromUuid(params.__pageUuid);
    }
    if (typeof params.tabId !== "number") {
      throw new Error("tabId is required");
    }
    return this.pageFromTabId(params.tabId);
  }

  private pageFromTabId(tabId: number): Page {
    const page = this.pages()[tabId - 1];
    if (!page) {
      throw new Error(`Tab not found: ${tabId}`);
    }
    return page;
  }

  private pageFromUuid(uuid: string): Page {
    if (uuid === UNRESOLVED_TAB) {
      // Pinning explicitly failed at enqueue time. Refuse to fall back to
      // any positional re-resolution — that would silently retarget the
      // action to a tab that did not exist when the caller submitted it.
      throw new Error("Tab closed before action could execute");
    }
    for (const page of this.pages()) {
      if (this.pageUuids.get(page) === uuid) {
        return page;
      }
    }
    throw new Error("Tab closed before action could execute");
  }

  private pages(): Page[] {
    return this.requireContext().pages();
  }

  private requireContext(): BrowserContext {
    if (!this.context) {
      throw new Error("Browser context is not initialized");
    }
    return this.context;
  }

  private resolveActiveTabId(): number {
    if (this.activeTabId !== null) {
      return this.activeTabId;
    }
    if (this.pages()[0]) {
      this.activeTabId = 1;
      return 1;
    }
    throw new Error("No active tab");
  }

  private tabIdForPage(page: Page): number {
    const index = this.pages().indexOf(page);
    if (index === -1) {
      throw new Error("Page is not part of the active context");
    }
    return index + 1;
  }

  private getPageState(page: Page): PageState {
    const state = this.pageStates.get(page);
    if (!state) {
      throw new Error("Page state is not initialized");
    }
    return state;
  }

  private async goto(page: Page, rawUrl: string): Promise<void> {
    const url = this.normalizeUrl(rawUrl);
    await page.goto(url, {
      waitUntil: "domcontentloaded",
      timeout: this.options.startupTimeoutMs,
    });
  }

  private startKeepalive(): void {
    const intervalMs = this.options.keepaliveIntervalMs;
    if (!intervalMs || intervalMs <= 0) {
      return;
    }
    this.keepaliveTimer = setInterval(() => {
      void this.runKeepalive();
    }, intervalMs);
    console.error(
      `[keepalive] enabled — refreshing pages every ${Math.round(
        intervalMs / 1000
      )}s`
    );
  }

  private stopKeepalive(): void {
    if (this.keepaliveTimer) {
      clearInterval(this.keepaliveTimer);
      this.keepaliveTimer = null;
    }
  }

  private async runKeepalive(): Promise<void> {
    if (!this.context) {
      return;
    }
    const pages = this.context.pages();
    for (const page of pages) {
      const url = page.url();
      if (!url || url === "about:blank") {
        continue;
      }
      try {
        await page.evaluate(() =>
          fetch(location.href, { credentials: "include" }).catch(() => {})
        );
      } catch {
        // Page may have been closed or navigating — ignore.
      }
    }
  }

  private normalizeUrl(rawUrl: string): string {
    if (!rawUrl || rawUrl === "about:blank") {
      return "about:blank";
    }
    if (/^[a-zA-Z][a-zA-Z\d+\-.]*:/.test(rawUrl)) {
      return rawUrl;
    }
    return `https://${rawUrl}`;
  }
}

function trimEntries(entries: Array<Record<string, unknown>>): void {
  if (entries.length > MAX_LOG_ENTRIES) {
    entries.splice(0, entries.length - MAX_LOG_ENTRIES);
  }
}

function trimNetwork(
  entries: NetworkEntry[],
  index: Map<Request, NetworkEntry>
): void {
  if (entries.length <= MAX_NETWORK_ENTRIES) {
    return;
  }
  const dropCount = entries.length - MAX_NETWORK_ENTRIES;
  const dropped = entries.splice(0, dropCount);
  if (dropped.length === 0) return;
  const droppedIds = new Set(dropped.map((entry) => entry.id));
  for (const [request, entry] of index) {
    if (droppedIds.has(entry.id)) {
      index.delete(request);
    }
  }
}

function sanitizeHeaders(
  headers: Record<string, string>
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(headers)) {
    const lower = key.toLowerCase();
    if (
      lower === "authorization" ||
      lower === "cookie" ||
      lower === "set-cookie" ||
      lower === "proxy-authorization"
    ) {
      out[lower] = "[redacted]";
      continue;
    }
    out[lower] = value;
  }
  return out;
}

function clipText(text: string | null, max: number): string | null {
  if (text === null) return null;
  if (text.length <= max) return text;
  return `${text.slice(0, max)}…[truncated]`;
}

function safeRegex(pattern: string): RegExp | null {
  try {
    return new RegExp(pattern);
  } catch {
    return null;
  }
}

function normalizeText(text: string): string {
  return text.replace(/\n{3,}/g, "\n\n").trim();
}

function cleanupHtml(html: string): string {
  return html
    .replace(/<script\b[^<]*(?:(?!<\/script>)<[^<]*)*<\/script>/gi, "")
    .replace(/<style\b[^<]*(?:(?!<\/style>)<[^<]*)*<\/style>/gi, "")
    .replace(/<svg\b[^<]*(?:(?!<\/svg>)<[^<]*)*<\/svg>/gi, "")
    .replace(/\sdata-[\w-]+=(["']).*?\1/gi, "");
}
