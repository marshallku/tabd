import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import WebSocket from "ws";
import type { BridgeAction, BridgeResponse } from "../../shared/protocol.js";
import type { BrowserDriver } from "../bridge.js";
import { getSecretStore } from "../secrets.js";
import { compileUrlMatcher, type UrlPatternType } from "../../shared/urlMatch.js";

type JsonObject = Record<string, unknown>;

interface CdpOptions {
  mode: "chromium-cdp" | "external-cdp";
  executablePath?: string;
  userDataDir?: string;
  debugPort: number;
  headless: boolean;
  startupTimeoutMs: number;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason?: unknown) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface CdpResponse {
  id?: number;
  result?: unknown;
  error?: { message?: string };
  method?: string;
  params?: JsonObject;
  sessionId?: string;
}

interface TargetInfo {
  targetId: string;
  title: string;
  url: string;
  attached?: boolean;
  type: string;
}

interface ConsoleEntry {
  level: string;
  text: string;
  timestamp: number;
}

interface ErrorEntry {
  message: string;
  source?: string;
  line?: number;
  column?: number;
  timestamp: number;
}

interface DialogInfo {
  type: string;
  message: string;
  defaultPrompt?: string;
  timestamp: number;
}

interface NetworkEntry {
  requestId: string;
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

interface TargetState {
  sessionId: string;
  consoleLogs: ConsoleEntry[];
  pageErrors: ErrorEntry[];
  lastDialog: DialogInfo | null;
  dialogBehavior: { action: "accept" | "dismiss"; text?: string };
  annotations: Map<number, string>;
  networkPending: number;
  networkLogs: NetworkEntry[];
  networkIndex: Map<string, NetworkEntry>;
}

const MAX_CONSOLE_ENTRIES = 200;
const MAX_ERROR_ENTRIES = 100;
const MAX_NETWORK_ENTRIES = 500;
const MAX_BODY_BYTES = 100_000;
const TEXT_CONTENT_TYPES =
  /^(text\/|application\/(json|xml|javascript|x-www-form-urlencoded|graphql))/i;
const CDP_TIMEOUT = 15_000;

export class CdpBrowserDriver implements BrowserDriver {
  private readonly options: CdpOptions;
  private child: ChildProcess | null = null;
  private socket: WebSocket | null = null;
  private nextId = 1;
  private activeTargetId: string | null = null;
  private readonly pending = new Map<number, PendingRequest>();
  private readonly sessions = new Map<string, TargetState>();
  private readonly eventHandlers = new Map<
    string,
    Array<(params: JsonObject, sessionId?: string) => void>
  >();

  constructor(options: CdpOptions) {
    this.options = options;
  }

  async init(): Promise<void> {
    if (this.options.mode === "chromium-cdp") {
      await this.launchBrowser();
    }

    const wsUrl = await this.getWebSocketUrl();
    await this.connect(wsUrl);
    await this.sendCommand("Target.setDiscoverTargets", { discover: true });

    this.on("Target.targetDestroyed", (params) => {
      const targetId = params.targetId as string;
      const state = this.sessions.get(targetId);
      if (state) {
        this.sessions.delete(targetId);
      }
      if (this.activeTargetId === targetId) {
        this.activeTargetId = null;
      }
    });
  }

  async close(): Promise<void> {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("Browser runtime closed"));
    }
    this.pending.clear();

    for (const [targetId, state] of this.sessions) {
      try {
        await this.sendCommand("Target.detachFromTarget", {
          sessionId: state.sessionId,
        });
      } catch {
        // ignore detach errors during shutdown
      }
    }
    this.sessions.clear();

    if (this.socket) {
      this.socket.close();
      this.socket = null;
    }

    if (this.child && !this.child.killed) {
      const child = this.child;
      this.child = null;
      // If the child already exited before we got here, no need to await.
      if (child.exitCode !== null || child.signalCode !== null) {
        return;
      }
      await new Promise<void>((resolve) => {
        let done = false;
        const finish = (): void => {
          if (done) return;
          done = true;
          resolve();
        };
        child.once("exit", finish);
        child.once("error", finish);
        const delivered = child.kill("SIGTERM");
        if (!delivered) {
          // kill returned false → process already gone or signal undeliverable.
          finish();
          return;
        }
        const force = setTimeout(() => {
          try {
            child.kill("SIGKILL");
          } catch {
            /* ignore */
          }
        }, 5000);
        const safety = setTimeout(finish, 10_000);
        child.once("exit", () => {
          clearTimeout(force);
          clearTimeout(safety);
        });
      });
    }
  }

  async execute(
    action: BridgeAction,
    params: Record<string, unknown>
  ): Promise<BridgeResponse> {
    const id = randomUUID();

    try {
      const data = await this.dispatch(action, params);
      return { id, success: true, data };
    } catch (error) {
      return {
        id,
        success: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  private async unsupported(message: string): Promise<never> {
    throw new Error(message);
  }

  // -- Event system --

  private on(
    method: string,
    handler: (params: JsonObject, sessionId?: string) => void
  ): void {
    const handlers = this.eventHandlers.get(method) ?? [];
    handlers.push(handler);
    this.eventHandlers.set(method, handlers);
  }

  private emit(method: string, params: JsonObject, sessionId?: string): void {
    const handlers = this.eventHandlers.get(method);
    if (handlers) {
      for (const handler of handlers) {
        handler(params, sessionId);
      }
    }
  }

  // -- Dispatch --

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
        return this.closeTab(await this.requireTargetId(params));
      case "tabs.navigate":
        return this.navigate(
          await this.resolveTargetId(params),
          String(params.url ?? "")
        );
      case "tabs.activate":
        return this.activateTab(await this.requireTargetId(params));
      case "tabs.goBack":
        return this.evaluate(
          await this.resolveTargetId(params),
          "history.back(); null;"
        );
      case "tabs.goForward":
        return this.evaluate(
          await this.resolveTargetId(params),
          "history.forward(); null;"
        );
      case "tabs.reload":
        return this.reload(await this.resolveTargetId(params));

      case "dom.getHtml":
        return this.getHtml(params);
      case "dom.getText":
        return this.getText(params);
      case "dom.contentSummary":
        return this.unsupported(
          "Content summary is not implemented in the CDP runtime"
        );
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
        return this.checkElement(params);
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
        return this.highlightElement(params);

      case "execution.executeJs":
        return this.evaluate(
          await this.resolveTargetId(params),
          String(params.code ?? "")
        );

      case "wait.selector":
        return this.waitForSelector(params);
      case "wait.navigation":
        return this.waitForNavigation(params);
      case "wait.networkIdle":
        return this.waitForNetworkIdle(params);
      case "wait.url":
        return this.waitForUrl(params);

      case "cookies.get":
        return this.getCookies(String(params.url ?? ""));
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
      case "secrets.delete":
      case "secrets.list":
        // Handled in bridge layer, never reaches the driver.
        throw new Error(`secrets actions are handled by the bridge`);

      default:
        throw new Error(`Unsupported action: ${action satisfies never}`);
    }
  }

  // -- Browser lifecycle --

  private async launchBrowser(): Promise<void> {
    const executable =
      this.options.executablePath ??
      [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
      ].find((candidate) => existsSync(candidate)) ??
      undefined;

    if (!executable) {
      throw new Error(
        "No Chromium-compatible browser found. Set BROWSER_EXECUTABLE or use BROWSER_RUNTIME=external-cdp."
      );
    }

    const args = [
      `--remote-debugging-port=${this.options.debugPort}`,
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-dev-shm-usage",
      "--disable-background-networking",
      "--disable-sync",
      "--disable-extensions",
      "--disable-gpu",
      "--no-sandbox",
      this.options.headless ? "--headless=new" : "",
      this.options.userDataDir
        ? `--user-data-dir=${this.options.userDataDir}`
        : "",
      "about:blank",
    ].filter(Boolean);

    this.child = spawn(executable, args, {
      stdio: "ignore",
      detached: false,
    });

    const deadline = Date.now() + this.options.startupTimeoutMs;
    while (Date.now() < deadline) {
      try {
        await this.getWebSocketUrl();
        return;
      } catch {
        await sleep(250);
      }
    }

    throw new Error("Timed out waiting for the Chromium CDP endpoint");
  }

  private async getWebSocketUrl(): Promise<string> {
    const response = await fetch(
      `http://127.0.0.1:${this.options.debugPort}/json/version`
    );

    if (!response.ok) {
      throw new Error(`CDP version endpoint returned ${response.status}`);
    }

    const payload = (await response.json()) as {
      webSocketDebuggerUrl?: string;
    };

    if (!payload.webSocketDebuggerUrl) {
      throw new Error(
        "CDP version endpoint did not return webSocketDebuggerUrl"
      );
    }

    return payload.webSocketDebuggerUrl;
  }

  private async connect(wsUrl: string): Promise<void> {
    this.socket = new WebSocket(wsUrl);

    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => {
        reject(new Error("Timed out connecting to the CDP websocket"));
      }, this.options.startupTimeoutMs);

      this.socket!.once("open", () => {
        clearTimeout(timeout);
        resolve();
      });
      this.socket!.once("error", (error) => {
        clearTimeout(timeout);
        reject(error);
      });
      this.socket!.on("message", (raw) => this.handleMessage(String(raw)));
    });
  }

  private handleMessage(raw: string): void {
    const message = JSON.parse(raw) as CdpResponse;

    if (typeof message.id === "number") {
      const pending = this.pending.get(message.id);
      if (!pending) {
        return;
      }

      clearTimeout(pending.timer);
      this.pending.delete(message.id);

      if (message.error?.message) {
        pending.reject(new Error(message.error.message));
        return;
      }

      pending.resolve(message.result);
      return;
    }

    if (message.method && message.params) {
      this.emit(message.method, message.params, message.sessionId);
    }
  }

  private async sendCommand(
    method: string,
    params: JsonObject = {},
    sessionId?: string
  ): Promise<unknown> {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
      throw new Error("CDP websocket is not connected");
    }

    const id = this.nextId++;
    const payload: JsonObject = { id, method, params };
    if (sessionId) {
      payload.sessionId = sessionId;
    }

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP command timed out: ${method}`));
      }, CDP_TIMEOUT);

      this.pending.set(id, { resolve, reject, timer });
      this.socket!.send(JSON.stringify(payload));
    });
  }

  // -- Session management (persistent per target) --

  private async getSession(targetId: string): Promise<TargetState> {
    const existing = this.sessions.get(targetId);
    if (existing) {
      return existing;
    }

    const result = (await this.sendCommand("Target.attachToTarget", {
      targetId,
      flatten: true,
    })) as { sessionId: string };

    const state: TargetState = {
      sessionId: result.sessionId,
      consoleLogs: [],
      pageErrors: [],
      lastDialog: null,
      dialogBehavior: { action: "accept" },
      annotations: new Map(),
      networkPending: 0,
      networkLogs: [],
      networkIndex: new Map(),
    };

    this.sessions.set(targetId, state);

    // Enable domains for this session
    await Promise.all([
      this.sendCommand("Runtime.enable", {}, result.sessionId),
      this.sendCommand("Page.enable", {}, result.sessionId),
      this.sendCommand("Network.enable", {}, result.sessionId),
      this.sendCommand("DOM.enable", {}, result.sessionId),
    ]);

    // Wire up event handlers for this session
    this.on("Runtime.consoleAPICalled", (params, sid) => {
      if (sid !== result.sessionId) return;
      const args =
        (params.args as Array<{ value?: unknown; description?: string }>) ?? [];
      const text = args
        .map((arg) =>
          arg.value !== undefined ? String(arg.value) : arg.description ?? ""
        )
        .join(" ");
      state.consoleLogs.push({
        level: String(params.type ?? "log"),
        text,
        timestamp: Date.now(),
      });
      if (state.consoleLogs.length > MAX_CONSOLE_ENTRIES) {
        state.consoleLogs.splice(
          0,
          state.consoleLogs.length - MAX_CONSOLE_ENTRIES
        );
      }
    });

    this.on("Runtime.exceptionThrown", (params, sid) => {
      if (sid !== result.sessionId) return;
      const detail = params.exceptionDetails as JsonObject | undefined;
      const exception = detail?.exception as JsonObject | undefined;
      state.pageErrors.push({
        message:
          (exception?.description as string) ??
          (detail?.text as string) ??
          "Unknown error",
        source: detail?.url as string | undefined,
        line: detail?.lineNumber as number | undefined,
        column: detail?.columnNumber as number | undefined,
        timestamp: Date.now(),
      });
      if (state.pageErrors.length > MAX_ERROR_ENTRIES) {
        state.pageErrors.splice(0, state.pageErrors.length - MAX_ERROR_ENTRIES);
      }
    });

    this.on("Page.javascriptDialogOpening", (params, sid) => {
      if (sid !== result.sessionId) return;
      state.lastDialog = {
        type: String(params.type ?? "alert"),
        message: String(params.message ?? ""),
        defaultPrompt: params.defaultPrompt as string | undefined,
        timestamp: Date.now(),
      };
      this.sendCommand(
        "Page.handleJavaScriptDialog",
        {
          accept: state.dialogBehavior.action === "accept",
          promptText: state.dialogBehavior.text,
        },
        result.sessionId
      ).catch(() => {
        // dialog may already be dismissed
      });
    });

    this.on("Network.requestWillBeSent", (params, sid) => {
      if (sid !== result.sessionId) return;
      state.networkPending++;
      const requestId = String(params.requestId ?? "");
      if (!requestId) return;
      const request = (params.request ?? {}) as JsonObject;
      const entry: NetworkEntry = {
        requestId,
        url: String(request.url ?? ""),
        method: String(request.method ?? "GET"),
        resourceType: String(params.type ?? "Other"),
        status: null,
        statusText: null,
        requestHeaders: sanitizeHeadersCdp(
          (request.headers as Record<string, string>) ?? {}
        ),
        responseHeaders: {},
        requestBody: clipTextCdp(
          typeof request.postData === "string" ? request.postData : null,
          MAX_BODY_BYTES
        ),
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
      state.networkIndex.set(requestId, entry);
      state.networkLogs.push(entry);
      trimNetworkCdp(state.networkLogs, state.networkIndex);
    });

    this.on("Network.responseReceived", (params, sid) => {
      if (sid !== result.sessionId) return;
      const requestId = String(params.requestId ?? "");
      const entry = state.networkIndex.get(requestId);
      if (!entry) return;
      const response = (params.response ?? {}) as JsonObject;
      entry.status =
        typeof response.status === "number" ? response.status : null;
      entry.statusText =
        typeof response.statusText === "string" ? response.statusText : null;
      entry.responseHeaders = sanitizeHeadersCdp(
        (response.headers as Record<string, string>) ?? {}
      );
      entry.fromCache = response.fromDiskCache === true;
    });

    this.on("Network.loadingFinished", (params, sid) => {
      if (sid !== result.sessionId) return;
      state.networkPending = Math.max(0, state.networkPending - 1);
      const requestId = String(params.requestId ?? "");
      const entry = state.networkIndex.get(requestId);
      if (!entry) return;
      entry.endTime = Date.now();
      entry.durationMs = entry.endTime - entry.startTime;
      const contentType = entry.responseHeaders["content-type"] ?? "";
      if (!TEXT_CONTENT_TYPES.test(contentType)) return;
      const encodedLength =
        typeof params.encodedDataLength === "number"
          ? params.encodedDataLength
          : Number(entry.responseHeaders["content-length"] ?? NaN);
      if (Number.isFinite(encodedLength) && encodedLength > MAX_BODY_BYTES) {
        entry.responseBodySize = encodedLength;
        entry.responseBodyTruncated = true;
        return;
      }
      this.sendCommand(
        "Network.getResponseBody",
        { requestId },
        result.sessionId
      )
        .then((value) => {
          const payload = value as
            | { body?: string; base64Encoded?: boolean }
            | undefined;
          if (!payload || typeof payload.body !== "string") return;
          const text = payload.base64Encoded
            ? Buffer.from(payload.body, "base64").toString("utf8")
            : payload.body;
          entry.responseBodySize = text.length;
          if (text.length > MAX_BODY_BYTES) {
            entry.responseBody = `${text.slice(0, MAX_BODY_BYTES)}…[truncated]`;
            entry.responseBodyTruncated = true;
          } else {
            entry.responseBody = text;
          }
        })
        .catch(() => {
          // body may not be retained — ignore
        });
    });

    this.on("Network.loadingFailed", (params, sid) => {
      if (sid !== result.sessionId) return;
      state.networkPending = Math.max(0, state.networkPending - 1);
      const requestId = String(params.requestId ?? "");
      const entry = state.networkIndex.get(requestId);
      if (!entry) return;
      entry.failed = true;
      entry.failureText =
        typeof params.errorText === "string" ? params.errorText : "failed";
      entry.endTime = Date.now();
      entry.durationMs = entry.endTime - entry.startTime;
    });

    return state;
  }

  // -- Tab management --

  private async listTabs(): Promise<Array<Record<string, unknown>>> {
    const result = (await this.sendCommand("Target.getTargets")) as {
      targetInfos?: TargetInfo[];
    };
    const tabs = (result.targetInfos ?? []).filter(
      (info) => info.type === "page"
    );
    if (!this.activeTargetId && tabs[0]) {
      this.activeTargetId = tabs[0].targetId;
    }
    return tabs.map((tab, index) => ({
      tabId: index + 1,
      targetId: tab.targetId,
      title: tab.title,
      url: tab.url,
      active: tab.targetId === this.activeTargetId,
    }));
  }

  private async openTab(url: string): Promise<Record<string, unknown>> {
    const result = (await this.sendCommand("Target.createTarget", {
      url,
    })) as { targetId: string };
    this.activeTargetId = result.targetId;
    // Eagerly attach so event capture starts immediately
    await this.getSession(result.targetId);
    const tabs = await this.listTabs();
    const tab = tabs.find((entry) => entry.targetId === result.targetId);
    return { tabId: tab?.tabId ?? 1, targetId: result.targetId, url };
  }

  private async closeTab(targetId: string): Promise<null> {
    this.sessions.delete(targetId);
    await this.sendCommand("Target.closeTarget", { targetId });
    if (this.activeTargetId === targetId) {
      this.activeTargetId = null;
    }
    return null;
  }

  private async navigate(
    targetId: string,
    url: string
  ): Promise<Record<string, unknown>> {
    const session = await this.getSession(targetId);
    await this.sendCommand("Page.navigate", { url }, session.sessionId);
    await this.waitForReadyState(targetId, 30_000);
    return { url };
  }

  private async activateTab(targetId: string): Promise<null> {
    await this.sendCommand("Target.activateTarget", { targetId });
    this.activeTargetId = targetId;
    return null;
  }

  private async reload(targetId: string): Promise<null> {
    const session = await this.getSession(targetId);
    await this.sendCommand("Page.reload", {}, session.sessionId);
    await this.waitForReadyState(targetId, 30_000);
    return null;
  }

  // -- DOM --

  private async getHtml(params: Record<string, unknown>): Promise<string> {
    const targetId = await this.resolveTargetId(params);
    const selector =
      typeof params.selector === "string" ? params.selector : "body";
    const outer = params.outer !== false;
    const clean = params.clean !== false;
    const code = `
            (() => {
                const node = document.querySelector(${JSON.stringify(
                  selector
                )});
                if (!node) throw new Error("Selector not found: ${selector}");
                const clone = node.cloneNode(true);
                if (${JSON.stringify(clean)}) {
                    clone.querySelectorAll("script,style,svg").forEach((el) => el.remove());
                    const walker = document.createTreeWalker(clone, NodeFilter.SHOW_COMMENT);
                    const comments = [];
                    while (walker.nextNode()) comments.push(walker.currentNode);
                    comments.forEach((node) => node.remove());
                    clone.querySelectorAll("*").forEach((el) => {
                        [...el.attributes]
                            .filter((attr) => attr.name.startsWith("data-"))
                            .forEach((attr) => el.removeAttribute(attr.name));
                    });
                }
                return ${outer ? "clone.outerHTML" : "clone.innerHTML"};
            })()
        `;
    return String(await this.evaluate(targetId, code));
  }

  private async getText(params: Record<string, unknown>): Promise<string> {
    const targetId = await this.resolveTargetId(params);
    const selector =
      typeof params.selector === "string"
        ? params.selector
        : "main, article, body";
    const raw = params.raw === true;
    const code = `
            (() => {
                const target = document.querySelector(${JSON.stringify(
                  selector
                )}) ?? document.body;
                if (${JSON.stringify(raw)}) return target.textContent ?? "";
                const text = target.innerText ?? target.textContent ?? "";
                return text.replace(/\\n{3,}/g, "\\n\\n").trim();
            })()
        `;
    return String(await this.evaluate(targetId, code));
  }

  private async querySelector(
    params: Record<string, unknown>
  ): Promise<Array<Record<string, unknown>>> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    const limit = Number(params.limit ?? 20);
    const visibleOnly = params.visibleOnly === true;
    const code = `
            (() => {
                return [...document.querySelectorAll(${JSON.stringify(
                  selector
                )})]
                    .filter((el) => {
                        if (!${JSON.stringify(visibleOnly)}) return true;
                        const rect = el.getBoundingClientRect();
                        const style = getComputedStyle(el);
                        return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
                    })
                    .slice(0, ${limit})
                    .map((el, index) => {
                        const rect = el.getBoundingClientRect();
                        return {
                            index,
                            tag: el.tagName.toLowerCase(),
                            id: el.id || null,
                            classes: [...el.classList],
                            text: (el.innerText || el.textContent || "").trim().slice(0, 200),
                            attributes: Object.fromEntries([...el.attributes].map((attr) => [attr.name, attr.value])),
                            rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
                        };
                    });
            })()
        `;
    return (await this.evaluate(targetId, code)) as Array<
      Record<string, unknown>
    >;
  }

  private async getFormValues(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    const code = `
            (() => {
                const form = document.querySelector(${JSON.stringify(
                  selector
                )});
                if (!(form instanceof HTMLFormElement)) throw new Error("Form not found: ${selector}");
                const data = {};
                for (const el of [...form.elements]) {
                    if (!(el instanceof HTMLElement) || !("name" in el) || !el.name) continue;
                    if (el instanceof HTMLInputElement && (el.type === "checkbox" || el.type === "radio")) {
                        data[el.name] = el.checked;
                    } else if ("value" in el) {
                        data[el.name] = el.value;
                    }
                }
                return data;
            })()
        `;
    return (await this.evaluate(targetId, code)) as Record<string, unknown>;
  }

  private async getAccessibilityTree(
    params: Record<string, unknown>
  ): Promise<string> {
    const targetId = await this.resolveTargetId(params);
    const maxElements = Number(params.maxElements ?? 500);
    const code = `
            (() => {
                const INTERACTIVE = new Set([
                    "A", "BUTTON", "INPUT", "SELECT", "TEXTAREA", "DETAILS", "SUMMARY",
                    "LABEL", "OPTION", "DIALOG", "MENU", "MENUITEM",
                ]);
                const LANDMARKS = new Set([
                    "MAIN", "NAV", "ASIDE", "HEADER", "FOOTER", "SECTION", "FORM", "SEARCH",
                ]);
                const results = [];
                let ref = 1;

                function walk(node, depth) {
                    if (results.length >= ${maxElements}) return;
                    if (!(node instanceof HTMLElement)) return;

                    const tag = node.tagName;
                    const role = node.getAttribute("role") || "";
                    const isInteractive = INTERACTIVE.has(tag) || node.isContentEditable ||
                        node.getAttribute("tabindex") !== null || role;
                    const isLandmark = LANDMARKS.has(tag);

                    if (isInteractive || isLandmark) {
                        const entry = {
                            ref: isInteractive ? ref++ : null,
                            tag: tag.toLowerCase(),
                            role: role || undefined,
                            name: node.getAttribute("aria-label") ||
                                  node.getAttribute("title") ||
                                  (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT"
                                      ? (node.labels?.[0]?.textContent?.trim() || node.getAttribute("placeholder") || node.getAttribute("name"))
                                      : (node.innerText || "").trim().slice(0, 80)) || undefined,
                            type: node instanceof HTMLInputElement ? node.type : undefined,
                            value: (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement || node instanceof HTMLSelectElement)
                                ? node.value?.slice(0, 200) : undefined,
                            checked: node instanceof HTMLInputElement && (node.type === "checkbox" || node.type === "radio")
                                ? node.checked : undefined,
                            disabled: node.hasAttribute("disabled") || undefined,
                            href: node instanceof HTMLAnchorElement ? node.href : undefined,
                            depth,
                        };
                        results.push(entry);
                    }

                    for (const child of node.children) {
                        walk(child, depth + (isLandmark ? 1 : 0));
                    }
                }

                walk(document.body, 0);

                return results.map((e) => {
                    const indent = "  ".repeat(e.depth);
                    const refStr = e.ref !== null ? "@" + e.ref + " " : "";
                    const parts = [refStr + e.tag];
                    if (e.role) parts.push("[" + e.role + "]");
                    if (e.name) parts.push(JSON.stringify(e.name));
                    if (e.type) parts.push("type=" + e.type);
                    if (e.value !== undefined) parts.push("value=" + JSON.stringify(e.value));
                    if (e.checked !== undefined) parts.push(e.checked ? "checked" : "unchecked");
                    if (e.disabled) parts.push("disabled");
                    if (e.href) parts.push("href=" + e.href);
                    return indent + parts.join(" ");
                }).join("\\n");
            })()
        `;
    return String(await this.evaluate(targetId, code));
  }

  // -- Interaction --

  private async waitForActionable(
    targetId: string,
    selector: string,
    options: {
      timeout?: number;
      requireEnabled?: boolean;
      requireHittable?: boolean;
      offsetX?: number;
      offsetY?: number;
    } = {}
  ): Promise<{ x: number; y: number; width: number; height: number }> {
    const timeout = options.timeout ?? 30_000;
    const requireEnabled = options.requireEnabled !== false;
    const requireHittable = options.requireHittable !== false;
    const offsetXParam = options.offsetX;
    const offsetYParam = options.offsetY;
    const deadline = Date.now() + timeout;
    let lastReason = "not-found";
    let lastRect: { x: number; y: number; w: number; h: number } | null = null;
    let stableSince = 0;

    while (Date.now() < deadline) {
      const probe = `(() => {
        const el = document.querySelector(${JSON.stringify(selector)});
        if (!el) return { ready: false, reason: 'not-found' };
        if (!(el instanceof Element)) return { ready: false, reason: 'not-element' };
        if (${JSON.stringify(requireEnabled)} && 'disabled' in el && el.disabled) {
          return { ready: false, reason: 'disabled' };
        }
        if ('inert' in el && el.inert) return { ready: false, reason: 'inert' };
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        const rect = el.getBoundingClientRect();
        if (rect.width === 0 || rect.height === 0) return { ready: false, reason: 'zero-size' };
        const cs = getComputedStyle(el);
        if (cs.visibility === 'hidden' || cs.display === 'none') return { ready: false, reason: 'hidden' };
        if (parseFloat(cs.opacity) === 0) return { ready: false, reason: 'transparent' };
        if (${JSON.stringify(requireHittable)}) {
          if (cs.pointerEvents === 'none') return { ready: false, reason: 'pointer-events-none' };
          const offsetX = ${offsetXParam === undefined ? "rect.width / 2" : JSON.stringify(offsetXParam)};
          const offsetY = ${offsetYParam === undefined ? "rect.height / 2" : JSON.stringify(offsetYParam)};
          const px = rect.x + offsetX;
          const py = rect.y + offsetY;
          if (px < 0 || py < 0 || px >= innerWidth || py >= innerHeight) {
            return { ready: false, reason: 'point-offscreen' };
          }
          const hit = document.elementFromPoint(px, py);
          if (!hit) return { ready: false, reason: 'no-hit' };
          if (hit !== el && !el.contains(hit)) {
            const tag = hit.tagName ? hit.tagName.toLowerCase() : '?';
            const id = hit.id ? '#' + hit.id : '';
            return { ready: false, reason: 'obstructed-by:' + tag + id };
          }
        }
        return {
          ready: true,
          rect: { x: rect.x, y: rect.y, w: rect.width, h: rect.height },
        };
      })()`;

      const result = (await this.evaluate(targetId, probe)) as
        | { ready: false; reason: string }
        | {
            ready: true;
            rect: { x: number; y: number; w: number; h: number };
          };

      if (!result.ready) {
        lastReason = result.reason;
        lastRect = null;
        stableSince = 0;
        await sleep(80);
        continue;
      }

      const r = result.rect;
      const same =
        lastRect !== null &&
        Math.abs(r.x - lastRect.x) < 0.5 &&
        Math.abs(r.y - lastRect.y) < 0.5 &&
        Math.abs(r.w - lastRect.w) < 0.5 &&
        Math.abs(r.h - lastRect.h) < 0.5;
      if (same) {
        if (Date.now() - stableSince >= 100) {
          return { x: r.x, y: r.y, width: r.w, height: r.h };
        }
      } else {
        lastRect = r;
        stableSince = Date.now();
      }
      await sleep(50);
    }

    throw new Error(
      `Element not actionable within ${timeout}ms (${selector}): ${lastReason}`
    );
  }

  private async dispatchMouseAt(
    targetId: string,
    type: "mouseMoved" | "mousePressed" | "mouseReleased",
    x: number,
    y: number,
    options: { button?: "left" | "right" | "middle"; clickCount?: number } = {}
  ): Promise<void> {
    const session = await this.getSession(targetId);
    await this.sendCommand(
      "Input.dispatchMouseEvent",
      {
        type,
        x,
        y,
        button: options.button ?? (type === "mouseMoved" ? "none" : "left"),
        clickCount: options.clickCount ?? (type === "mouseMoved" ? 0 : 1),
      },
      session.sessionId
    );
  }

  private async click(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    if (!selector) throw new Error("selector is required");
    const offsetX = typeof params.x === "number" ? params.x : undefined;
    const offsetY = typeof params.y === "number" ? params.y : undefined;
    const rect = await this.waitForActionable(targetId, selector, {
      offsetX,
      offsetY,
    });
    const x = rect.x + (offsetX ?? rect.width / 2);
    const y = rect.y + (offsetY ?? rect.height / 2);
    await this.dispatchMouseAt(targetId, "mouseMoved", x, y);
    await this.dispatchMouseAt(targetId, "mousePressed", x, y);
    await this.dispatchMouseAt(targetId, "mouseReleased", x, y);
    return null;
  }

  private async typeText(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    if (!selector) throw new Error("selector is required");
    await this.waitForActionable(targetId, selector, { requireHittable: false });
    const text = String(params.text ?? "");
    const clear = params.clear !== false;
    await this.fillEditable(targetId, selector, text, clear);
    return null;
  }

  private async typeSecret(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    if (!selector) throw new Error("selector is required");
    const secretId = String(params.secretId ?? "");
    if (!secretId) throw new Error("secretId is required");
    const secret = await getSecretStore().get(secretId);
    await this.waitForActionable(targetId, selector, { requireHittable: false });
    const clear = params.clear !== false;
    await this.fillEditable(targetId, selector, secret, clear);
    return null;
  }

  private async fillEditable(
    targetId: string,
    selector: string,
    text: string,
    clear: boolean
  ): Promise<void> {
    const code = `
      (() => {
        const el = document.querySelector(${JSON.stringify(selector)});
        if (!(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLElement && el.isContentEditable)) {
          throw new Error("Type target not found or not editable: ${selector}");
        }
        el.scrollIntoView({ block: "center", inline: "center" });
        el.focus();
        if (${JSON.stringify(clear)}) {
          if ("value" in el) el.value = "";
          else el.textContent = "";
        }
        if ("value" in el) el.value = ${JSON.stringify(text)};
        else document.execCommand("insertText", false, ${JSON.stringify(text)});
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
        return null;
      })()
    `;
    await this.evaluate(targetId, code);
  }

  private async scroll(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector =
      typeof params.selector === "string"
        ? JSON.stringify(params.selector)
        : "null";
    const x = Number(params.x ?? 0);
    const y = Number(params.y ?? 0);
    const code = `
            (() => {
                const selector = ${selector};
                if (selector) {
                    const el = document.querySelector(selector);
                    if (!(el instanceof HTMLElement)) throw new Error("Scroll target not found");
                    el.scrollIntoView({ block: "center", inline: "center" });
                    return null;
                }
                window.scrollBy(${x}, ${y});
                return null;
            })()
        `;
    await this.evaluate(targetId, code);
    return null;
  }

  private async pressKey(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const key = String(params.key ?? "");
    const selector =
      typeof params.selector === "string" ? params.selector : null;

    if (selector) {
      await this.evaluate(
        targetId,
        `(() => {
                    const el = document.querySelector(${JSON.stringify(
                      selector
                    )});
                    if (el instanceof HTMLElement) el.focus();
                })()`
      );
    }

    const session = await this.getSession(targetId);
    const keyDef = resolveKey(key);

    await this.sendCommand(
      "Input.dispatchKeyEvent",
      {
        type: "keyDown",
        key: keyDef.key,
        code: keyDef.code,
        text: keyDef.text,
        windowsVirtualKeyCode: keyDef.keyCode,
        nativeVirtualKeyCode: keyDef.keyCode,
      },
      session.sessionId
    );

    await this.sendCommand(
      "Input.dispatchKeyEvent",
      {
        type: "keyUp",
        key: keyDef.key,
        code: keyDef.code,
        windowsVirtualKeyCode: keyDef.keyCode,
        nativeVirtualKeyCode: keyDef.keyCode,
      },
      session.sessionId
    );

    return null;
  }

  private async hover(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    if (!selector) {
      throw new Error("selector is required");
    }
    const offsetX = typeof params.x === "number" ? params.x : undefined;
    const offsetY = typeof params.y === "number" ? params.y : undefined;
    const rect = await this.waitForActionable(targetId, selector, {
      requireEnabled: false,
      offsetX,
      offsetY,
    });
    await this.dispatchMouseAt(
      targetId,
      "mouseMoved",
      rect.x + (offsetX ?? rect.width / 2),
      rect.y + (offsetY ?? rect.height / 2)
    );
    return null;
  }

  private async mouseMove(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const x = Number(params.x ?? 0);
    const y = Number(params.y ?? 0);
    await this.dispatchMouseAt(targetId, "mouseMoved", x, y);
    return null;
  }

  private async selectOption(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    if (!selector) throw new Error("selector is required");
    await this.waitForActionable(targetId, selector, { requireHittable: false });
    const value = typeof params.value === "string" ? params.value : null;
    const label = typeof params.label === "string" ? params.label : null;
    const index = typeof params.index === "number" ? params.index : null;
    const code = `
            (() => {
                const el = document.querySelector(${JSON.stringify(selector)});
                if (!(el instanceof HTMLSelectElement)) throw new Error("Select not found: ${selector}");
                const options = [...el.options];
                const target =
                    ${JSON.stringify(
                      value
                    )} !== null ? options.find((opt) => opt.value === ${JSON.stringify(
      value
    )}) :
                    ${JSON.stringify(
                      label
                    )} !== null ? options.find((opt) => opt.label === ${JSON.stringify(
      label
    )}) :
                    ${index === null ? "null" : `options[${index}]`};
                if (!target) throw new Error("Requested option was not found");
                el.value = target.value;
                el.dispatchEvent(new Event("input", { bubbles: true }));
                el.dispatchEvent(new Event("change", { bubbles: true }));
                return null;
            })()
        `;
    await this.evaluate(targetId, code);
    return null;
  }

  private async checkElement(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    if (!selector) throw new Error("selector is required");
    await this.waitForActionable(targetId, selector, { requireHittable: false });
    const checked = params.checked !== false;
    const code = `
            (() => {
                const el = document.querySelector(${JSON.stringify(selector)});
                if (!(el instanceof HTMLInputElement)) throw new Error("Checkbox/radio not found: ${selector}");
                el.checked = ${JSON.stringify(checked)};
                el.dispatchEvent(new Event("input", { bubbles: true }));
                el.dispatchEvent(new Event("change", { bubbles: true }));
                return null;
            })()
        `;
    await this.evaluate(targetId, code);
    return null;
  }

  private async clickAnnotation(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const ref = Number(params.ref ?? 0);
    const state = await this.getSession(targetId);
    const selector = state.annotations.get(ref);
    if (!selector) {
      throw new Error(`Annotation @${ref} not found. Run annotate_page first.`);
    }
    return this.click({ ...params, selector });
  }

  private async typeAnnotation(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const ref = Number(params.ref ?? 0);
    const state = await this.getSession(targetId);
    const selector = state.annotations.get(ref);
    if (!selector) {
      throw new Error(`Annotation @${ref} not found. Run annotate_page first.`);
    }
    return this.typeText({ ...params, selector });
  }

  // -- Capture --

  private async captureScreenshot(
    params: Record<string, unknown>
  ): Promise<string> {
    const targetId = await this.resolveTargetId(params);
    const session = await this.getSession(targetId);
    const result = (await this.sendCommand(
      "Page.captureScreenshot",
      { format: "png" },
      session.sessionId
    )) as { data: string };
    return `data:image/png;base64,${result.data}`;
  }

  private async getComputedStyles(
    params: Record<string, unknown>
  ): Promise<Record<string, string>> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    const properties = Array.isArray(params.properties)
      ? params.properties.map(String)
      : null;
    const code = `
            (() => {
                const el = document.querySelector(${JSON.stringify(selector)});
                if (!(el instanceof Element)) throw new Error("Element not found: ${selector}");
                const style = getComputedStyle(el);
                const keys = ${JSON.stringify(properties)} ?? [...style];
                return Object.fromEntries(keys.map((key) => [key, style.getPropertyValue(key)]));
            })()
        `;
    return (await this.evaluate(targetId, code)) as Record<string, string>;
  }

  private async getElementRect(
    params: Record<string, unknown>
  ): Promise<Record<string, number>> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    const code = `
            (() => {
                const el = document.querySelector(${JSON.stringify(selector)});
                if (!(el instanceof Element)) throw new Error("Element not found: ${selector}");
                el.scrollIntoView({ block: "center", inline: "center" });
                const rect = el.getBoundingClientRect();
                return {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    devicePixelRatio: window.devicePixelRatio || 1
                };
            })()
        `;
    return (await this.evaluate(targetId, code)) as Record<string, number>;
  }

  private async getPageMetrics(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const targetId = await this.resolveTargetId(params);
    const code = `
            (() => {
                const nav = performance.getEntriesByType("navigation")[0];
                return {
                    url: location.href,
                    title: document.title,
                    readyState: document.readyState,
                    domNodes: document.getElementsByTagName("*").length,
                    resources: performance.getEntriesByType("resource").length,
                    navigation: nav ? {
                        type: nav.type,
                        domContentLoaded: nav.domContentLoadedEventEnd,
                        loadEventEnd: nav.loadEventEnd
                    } : null
                };
            })()
        `;
    return (await this.evaluate(targetId, code)) as Record<string, unknown>;
  }

  private async annotatePage(
    params: Record<string, unknown>
  ): Promise<{ count: number }> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    state.annotations.clear();

    const code = `
            (() => {
                const INTERACTIVE = "a, button, input, select, textarea, [role='button'], [role='link'], [role='tab'], [tabindex], [contenteditable='true']";
                const elements = [...document.querySelectorAll(INTERACTIVE)]
                    .filter((el) => {
                        const rect = el.getBoundingClientRect();
                        const style = getComputedStyle(el);
                        return rect.width > 0 && rect.height > 0 &&
                            style.display !== "none" && style.visibility !== "hidden" &&
                            parseFloat(style.opacity) > 0;
                    });

                const annotations = [];
                elements.forEach((el, i) => {
                    const ref = i + 1;
                    const rect = el.getBoundingClientRect();
                    const badge = document.createElement("div");
                    badge.className = "__ai_annotation__";
                    badge.textContent = String(ref);
                    Object.assign(badge.style, {
                        position: "fixed",
                        left: rect.left + "px",
                        top: rect.top + "px",
                        background: "#e53e3e",
                        color: "#fff",
                        fontSize: "11px",
                        fontWeight: "bold",
                        padding: "1px 4px",
                        borderRadius: "3px",
                        zIndex: "2147483647",
                        pointerEvents: "none",
                        lineHeight: "1.3",
                    });
                    document.body.appendChild(badge);

                    // Build a unique selector for this element
                    let selector = "";
                    if (el.id) {
                        selector = "#" + CSS.escape(el.id);
                    } else {
                        const tag = el.tagName.toLowerCase();
                        const parent = el.parentElement;
                        if (parent) {
                            const siblings = [...parent.children].filter((s) => s.tagName === el.tagName);
                            const idx = siblings.indexOf(el);
                            selector = tag + ":nth-of-type(" + (idx + 1) + ")";
                            // Walk up to build a unique path
                            let current = parent;
                            let path = selector;
                            for (let d = 0; d < 3 && current && current !== document.body; d++) {
                                const pTag = current.tagName.toLowerCase();
                                if (current.id) {
                                    path = "#" + CSS.escape(current.id) + " > " + path;
                                    break;
                                }
                                const pParent = current.parentElement;
                                if (pParent) {
                                    const pSiblings = [...pParent.children].filter((s) => s.tagName === current.tagName);
                                    const pIdx = pSiblings.indexOf(current);
                                    path = pTag + ":nth-of-type(" + (pIdx + 1) + ") > " + path;
                                }
                                current = pParent;
                            }
                            selector = path;
                        } else {
                            selector = tag;
                        }
                    }

                    annotations.push({ ref, selector });
                });
                return annotations;
            })()
        `;

    const annotations = (await this.evaluate(targetId, code)) as Array<{
      ref: number;
      selector: string;
    }>;

    for (const { ref, selector } of annotations) {
      state.annotations.set(ref, selector);
    }

    return { count: annotations.length };
  }

  private async clearAnnotations(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    state.annotations.clear();
    await this.evaluate(
      targetId,
      `document.querySelectorAll(".__ai_annotation__").forEach((el) => el.remove()); null;`
    );
    return null;
  }

  private async highlightElement(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    const color = String(params.color ?? "rgba(229, 62, 62, 0.3)");
    const duration = Number(params.duration ?? 3000);
    const code = `
            (() => {
                const el = document.querySelector(${JSON.stringify(selector)});
                if (!(el instanceof Element)) throw new Error("Element not found: ${selector}");
                const rect = el.getBoundingClientRect();
                const overlay = document.createElement("div");
                overlay.className = "__ai_highlight__";
                Object.assign(overlay.style, {
                    position: "fixed",
                    left: rect.left + "px",
                    top: rect.top + "px",
                    width: rect.width + "px",
                    height: rect.height + "px",
                    background: ${JSON.stringify(color)},
                    border: "2px solid rgba(229, 62, 62, 0.8)",
                    zIndex: "2147483646",
                    pointerEvents: "none",
                    transition: "opacity 0.3s",
                });
                document.body.appendChild(overlay);
                setTimeout(() => {
                    overlay.style.opacity = "0";
                    setTimeout(() => overlay.remove(), 300);
                }, ${duration});
                return null;
            })()
        `;
    await this.evaluate(targetId, code);
    return null;
  }

  // -- Wait --

  private async waitForSelector(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const selector = String(params.selector ?? "");
    const visible = params.visible === true;
    const timeout = Number(params.timeout ?? 10_000);
    const deadline = Date.now() + timeout;

    while (Date.now() < deadline) {
      try {
        const found = await this.evaluate(
          targetId,
          `
                    (() => {
                        const el = document.querySelector(${JSON.stringify(
                          selector
                        )});
                        if (!el) return false;
                        if (!${JSON.stringify(visible)}) return true;
                        const rect = el.getBoundingClientRect();
                        const style = getComputedStyle(el);
                        return rect.width > 0 && rect.height > 0 && style.display !== "none" && style.visibility !== "hidden";
                    })()
                    `
        );
        if (found === true) {
          return null;
        }
      } catch {
        // keep polling while the page is navigating
      }
      await sleep(200);
    }

    throw new Error(`Timed out waiting for selector: ${selector}`);
  }

  private async waitForNavigation(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    await this.waitForReadyState(targetId, Number(params.timeout ?? 30_000));
    return null;
  }

  private async waitForNetworkIdle(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const timeout = Number(params.timeout ?? 10_000);
    const idleTime = Number(params.idleTime ?? 500);
    const state = await this.getSession(targetId);
    const deadline = Date.now() + timeout;

    let idleSince: number | null = null;

    while (Date.now() < deadline) {
      if (state.networkPending <= 0) {
        if (idleSince === null) {
          idleSince = Date.now();
        } else if (Date.now() - idleSince >= idleTime) {
          return null;
        }
      } else {
        idleSince = null;
      }
      await sleep(100);
    }

    throw new Error(
      `Timed out waiting for network idle (${state.networkPending} pending requests)`
    );
  }

  private async waitForUrl(
    params: Record<string, unknown>
  ): Promise<{ url: string }> {
    const targetId = await this.resolveTargetId(params);
    const pattern = String(params.pattern ?? "");
    const patternType = (params.patternType as UrlPatternType) ?? "exact";
    const timeout = Number(params.timeout ?? 30_000);
    const match = compileUrlMatcher(pattern, patternType);
    const deadline = Date.now() + timeout;
    let lastUrl = "";

    while (Date.now() < deadline) {
      try {
        lastUrl = String(
          (await this.evaluate(targetId, "location.href")) ?? ""
        );
        if (match(lastUrl)) {
          return { url: lastUrl };
        }
      } catch {
        // page is navigating; keep polling
      }
      await sleep(200);
    }

    throw new Error(
      `Timed out waiting for URL pattern (${patternType}) ${pattern}; last seen: ${lastUrl}`
    );
  }

  // -- Cookies --

  private async getCookies(url: string): Promise<unknown> {
    const result = (await this.sendCommand("Network.getCookies", {
      urls: [url],
    })) as { cookies?: unknown };
    return result.cookies ?? [];
  }

  private async setCookie(params: Record<string, unknown>): Promise<null> {
    const result = (await this.sendCommand("Network.setCookie", {
      url: params.url,
      name: params.name,
      value: params.value,
      domain: params.domain,
      path: params.path,
      secure: params.secure,
      httpOnly: params.httpOnly,
      expires: params.expirationDate,
    })) as { success?: boolean };
    if (!result.success) {
      throw new Error("CDP rejected the cookie");
    }
    return null;
  }

  private async deleteCookie(params: Record<string, unknown>): Promise<null> {
    await this.sendCommand("Network.deleteCookies", {
      url: params.url,
      name: params.name,
    });
    return null;
  }

  // -- Storage --

  private async getStorage(
    params: Record<string, unknown>
  ): Promise<Record<string, string> | string | null> {
    const targetId = await this.resolveTargetId(params);
    const storageType =
      params.type === "session" ? "sessionStorage" : "localStorage";
    const key = typeof params.key === "string" ? params.key : null;
    const code = key
      ? `${storageType}.getItem(${JSON.stringify(key)})`
      : `Object.fromEntries(Object.keys(${storageType}).map((key) => [key, ${storageType}.getItem(key) ?? ""]))`;
    return (await this.evaluate(targetId, code)) as
      | Record<string, string>
      | string
      | null;
  }

  private async setStorage(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const storageType =
      params.type === "session" ? "sessionStorage" : "localStorage";
    await this.evaluate(
      targetId,
      `${storageType}.setItem(${JSON.stringify(
        String(params.key ?? "")
      )}, ${JSON.stringify(String(params.value ?? ""))}); null;`
    );
    return null;
  }

  private async clearStorage(params: Record<string, unknown>): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const storageType =
      params.type === "session" ? "sessionStorage" : "localStorage";
    await this.evaluate(targetId, `${storageType}.clear(); null;`);
    return null;
  }

  // -- Dialog --

  private async setDialogBehavior(
    params: Record<string, unknown>
  ): Promise<null> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    state.dialogBehavior = {
      action: params.action === "dismiss" ? "dismiss" : "accept",
      text: typeof params.text === "string" ? params.text : undefined,
    };
    return null;
  }

  private async getLastDialog(
    params: Record<string, unknown>
  ): Promise<DialogInfo | null> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    return state.lastDialog;
  }

  // -- Monitor --

  private async getConsoleLogs(
    params: Record<string, unknown>
  ): Promise<ConsoleEntry[]> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    const level = String(params.level ?? "all");
    const limit = Number(params.limit ?? 100);
    let logs = state.consoleLogs;
    if (level !== "all") {
      logs = logs.filter((entry) => entry.level === level);
    }
    return logs.slice(-limit);
  }

  private async getPageErrors(
    params: Record<string, unknown>
  ): Promise<ErrorEntry[]> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    const limit = Number(params.limit ?? 50);
    return state.pageErrors.slice(-limit);
  }

  private async getNetworkLogs(
    params: Record<string, unknown>
  ): Promise<NetworkEntry[]> {
    const targetId = await this.resolveTargetId(params);
    const state = await this.getSession(targetId);
    const limit = Number(params.limit ?? 100);
    const method =
      typeof params.method === "string" ? params.method.toUpperCase() : null;
    const statusParam = params.status;
    const urlPattern =
      typeof params.urlPattern === "string" && params.urlPattern.length > 0
        ? safeRegexCdp(params.urlPattern)
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

  // -- Evaluate (uses persistent session) --

  private async evaluate(
    targetId: string,
    expression: string
  ): Promise<unknown> {
    const session = await this.getSession(targetId);
    const result = (await this.sendCommand(
      "Runtime.evaluate",
      {
        expression,
        awaitPromise: true,
        returnByValue: true,
      },
      session.sessionId
    )) as {
      result?: { value?: unknown; description?: string };
      exceptionDetails?: {
        text?: string;
        exception?: { description?: string };
      };
    };
    if (result.exceptionDetails) {
      const message =
        result.exceptionDetails.exception?.description ??
        result.exceptionDetails.text ??
        "Evaluation failed";
      throw new Error(message);
    }
    return result.result?.value;
  }

  private async waitForReadyState(
    targetId: string,
    timeout: number
  ): Promise<void> {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      try {
        const readyState = await this.evaluate(targetId, "document.readyState");
        if (readyState === "interactive" || readyState === "complete") {
          return;
        }
      } catch {
        // keep polling during navigation
      }
      await sleep(200);
    }
    throw new Error("Timed out waiting for navigation");
  }

  // -- Target resolution --

  private requireTabId(params: Record<string, unknown>): number {
    if (typeof params.tabId !== "number" || !Number.isInteger(params.tabId)) {
      throw new Error("tabId is required");
    }
    return params.tabId;
  }

  private async requireTargetId(
    params: Record<string, unknown>
  ): Promise<string> {
    return this.targetIdFromTabId(this.requireTabId(params));
  }

  private async resolveTargetId(
    params: Record<string, unknown>
  ): Promise<string> {
    if (typeof params.tabId === "number" && Number.isInteger(params.tabId)) {
      return this.targetIdFromTabId(params.tabId);
    }
    if (this.activeTargetId) {
      return this.activeTargetId;
    }
    const tabs = await this.listTabs();
    const first = tabs[0];
    if (!first || typeof first.targetId !== "string") {
      throw new Error("No browser tabs are open");
    }
    this.activeTargetId = first.targetId;
    return first.targetId;
  }

  private async targetIdFromTabId(tabId: number): Promise<string> {
    const tabs = await this.listTabs();
    const tab = tabs.find((entry) => entry.tabId === tabId);
    if (!tab || typeof tab.targetId !== "string") {
      throw new Error(`Tab not found: ${tabId}`);
    }
    return tab.targetId;
  }
}

function trimNetworkCdp(
  entries: NetworkEntry[],
  index: Map<string, NetworkEntry>
): void {
  if (entries.length <= MAX_NETWORK_ENTRIES) return;
  const dropCount = entries.length - MAX_NETWORK_ENTRIES;
  const dropped = entries.splice(0, dropCount);
  for (const entry of dropped) {
    index.delete(entry.requestId);
  }
}

function sanitizeHeadersCdp(
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

function clipTextCdp(text: string | null, max: number): string | null {
  if (text === null) return null;
  if (text.length <= max) return text;
  return `${text.slice(0, max)}…[truncated]`;
}

function safeRegexCdp(pattern: string): RegExp | null {
  try {
    return new RegExp(pattern);
  } catch {
    return null;
  }
}

// -- Key mapping --

interface KeyDef {
  key: string;
  code: string;
  keyCode: number;
  text: string;
}

function resolveKey(input: string): KeyDef {
  const special: Record<string, KeyDef> = {
    Enter: { key: "Enter", code: "Enter", keyCode: 13, text: "\r" },
    Tab: { key: "Tab", code: "Tab", keyCode: 9, text: "" },
    Escape: { key: "Escape", code: "Escape", keyCode: 27, text: "" },
    Backspace: { key: "Backspace", code: "Backspace", keyCode: 8, text: "" },
    Delete: { key: "Delete", code: "Delete", keyCode: 46, text: "" },
    ArrowUp: { key: "ArrowUp", code: "ArrowUp", keyCode: 38, text: "" },
    ArrowDown: { key: "ArrowDown", code: "ArrowDown", keyCode: 40, text: "" },
    ArrowLeft: { key: "ArrowLeft", code: "ArrowLeft", keyCode: 37, text: "" },
    ArrowRight: {
      key: "ArrowRight",
      code: "ArrowRight",
      keyCode: 39,
      text: "",
    },
    Home: { key: "Home", code: "Home", keyCode: 36, text: "" },
    End: { key: "End", code: "End", keyCode: 35, text: "" },
    PageUp: { key: "PageUp", code: "PageUp", keyCode: 33, text: "" },
    PageDown: { key: "PageDown", code: "PageDown", keyCode: 34, text: "" },
    Space: { key: " ", code: "Space", keyCode: 32, text: " " },
    F1: { key: "F1", code: "F1", keyCode: 112, text: "" },
    F2: { key: "F2", code: "F2", keyCode: 113, text: "" },
    F3: { key: "F3", code: "F3", keyCode: 114, text: "" },
    F4: { key: "F4", code: "F4", keyCode: 115, text: "" },
    F5: { key: "F5", code: "F5", keyCode: 116, text: "" },
    F6: { key: "F6", code: "F6", keyCode: 117, text: "" },
    F7: { key: "F7", code: "F7", keyCode: 118, text: "" },
    F8: { key: "F8", code: "F8", keyCode: 119, text: "" },
    F9: { key: "F9", code: "F9", keyCode: 120, text: "" },
    F10: { key: "F10", code: "F10", keyCode: 121, text: "" },
    F11: { key: "F11", code: "F11", keyCode: 122, text: "" },
    F12: { key: "F12", code: "F12", keyCode: 123, text: "" },
  };

  if (special[input]) {
    return special[input];
  }

  // Single character
  const char = input.length === 1 ? input : input.toLowerCase();
  return {
    key: char,
    code: `Key${char.toUpperCase()}`,
    keyCode: char.toUpperCase().charCodeAt(0),
    text: char,
  };
}
