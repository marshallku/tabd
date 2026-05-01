import { randomUUID } from "node:crypto";
import type { BridgeAction, BridgeResponse } from "../../shared/protocol.js";
import type { BrowserDriver } from "../bridge.js";
import {
  cleanHtml,
  domainMatches,
  extractTitle,
  htmlToText,
  parseSetCookie,
  type CookieEntry,
} from "../utils/fetchContent.js";

interface FetchTab {
  tabId: number;
  url: string;
  title: string;
  html: string;
  status?: number;
  contentType?: string;
  fetchedAt?: number;
  localStorage: Map<string, string>;
  sessionStorage: Map<string, string>;
}

export class FetchBrowserDriver implements BrowserDriver {
  private tabs = new Map<number, FetchTab>();
  private nextTabId = 1;
  private activeTabId: number | null = null;
  private cookieJar = new Map<string, CookieEntry[]>();

  async init(): Promise<void> {
    await this.openTab("about:blank");
  }

  async close(): Promise<void> {
    this.tabs.clear();
    this.activeTabId = null;
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

  private async dispatch(
    action: BridgeAction,
    params: Record<string, unknown>
  ): Promise<unknown> {
    switch (action) {
      case "tabs.list":
        return [...this.tabs.values()].map((tab) => ({
          tabId: tab.tabId,
          title: tab.title,
          url: tab.url,
          active: tab.tabId === this.activeTabId,
          status: tab.status ?? null,
        }));
      case "tabs.open":
        return this.openTab(String(params.url ?? "about:blank"));
      case "tabs.close":
        return this.closeTab(this.requireTabId(params));
      case "tabs.navigate":
        return this.navigate(
          this.resolveTabId(params),
          String(params.url ?? "")
        );
      case "tabs.activate":
        return this.activateTab(this.requireTabId(params));
      case "tabs.goBack":
      case "tabs.goForward":
        return this.unsupported(
          "History navigation is not available in http-fetch runtime"
        );
      case "tabs.reload":
        return this.navigate(
          this.resolveTabId(params),
          this.getTab(params).url
        );
      case "dom.getHtml":
        return this.getHtml(params);
      case "dom.getText":
        return this.getText(params);
      case "dom.contentSummary":
        return this.getContentSummary(params);
      case "dom.querySelector":
        return this.unsupported("Selector queries require a DOM runtime");
      case "dom.formValues":
        return this.unsupported("Form introspection requires a DOM runtime");
      case "dom.accessibilityTree":
        return this.unsupported("Accessibility tree requires a DOM runtime");
      case "interaction.click":
      case "interaction.type":
      case "interaction.typeSecret":
      case "interaction.scroll":
      case "interaction.pressKey":
      case "interaction.hover":
      case "interaction.mouseMove":
      case "interaction.selectOption":
      case "interaction.check":
      case "interaction.clickAnnotation":
      case "interaction.typeAnnotation":
        return this.unsupported(
          "Interactive control is not available in http-fetch runtime"
        );
      case "capture.screenshot":
      case "capture.computedStyles":
      case "capture.annotate":
      case "capture.clearAnnotations":
      case "capture.highlight":
      case "capture.elementRect":
        return this.unsupported("Visual capture requires a browser runtime");
      case "execution.executeJs":
        return this.unsupported(
          "JavaScript execution requires a browser runtime"
        );
      case "wait.selector":
        return this.unsupported("Selector waits require a browser runtime");
      case "wait.navigation":
        return null;
      case "wait.networkIdle":
        return null;
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
      case "monitor.consoleLogs":
      case "monitor.pageErrors":
      case "monitor.networkLogs":
        return this.unsupported("Runtime logs require a browser runtime");
      case "capture.metrics":
        return this.getMetrics(params);
      case "dialog.setBehavior":
      case "dialog.getLast":
        return this.unsupported("Dialogs require a browser runtime");
      case "secrets.put":
      case "secrets.delete":
        // Handled in bridge layer, never reaches the driver.
        return this.unsupported("secrets actions are handled by the bridge");
      default:
        return this.unsupported(
          `Unsupported action: ${action satisfies never}`
        );
    }
  }

  private async openTab(url: string): Promise<Record<string, unknown>> {
    const tabId = this.nextTabId++;
    const tab: FetchTab = {
      tabId,
      url: "about:blank",
      title: "about:blank",
      html: "",
      localStorage: new Map(),
      sessionStorage: new Map(),
    };
    this.tabs.set(tabId, tab);
    this.activeTabId = tabId;
    if (url !== "about:blank") {
      await this.navigate(tabId, url);
    }
    return { tabId, url: tab.url, title: tab.title };
  }

  private async closeTab(tabId: number): Promise<null> {
    if (!this.tabs.delete(tabId)) {
      throw new Error(`Tab not found: ${tabId}`);
    }
    if (this.activeTabId === tabId) {
      this.activeTabId = this.tabs.keys().next().value ?? null;
    }
    return null;
  }

  private async activateTab(tabId: number): Promise<null> {
    this.getTab({ tabId });
    this.activeTabId = tabId;
    return null;
  }

  private async navigate(
    tabId: number,
    rawUrl: string
  ): Promise<Record<string, unknown>> {
    const tab = this.getTab({ tabId });
    const url = this.normalizeUrl(rawUrl);

    if (url === "about:blank") {
      tab.url = url;
      tab.title = "about:blank";
      tab.html = "";
      tab.status = undefined;
      tab.contentType = undefined;
      tab.fetchedAt = Date.now();
      return { tabId, url, title: tab.title };
    }

    const response = await fetch(url, {
      headers: this.buildHeaders(url),
      redirect: "follow",
    });

    const html = await response.text();
    this.storeSetCookies(url, response);

    tab.url = response.url || url;
    tab.html = html;
    tab.status = response.status;
    tab.contentType = response.headers.get("content-type") ?? undefined;
    tab.title = extractTitle(html) || response.url || url;
    tab.fetchedAt = Date.now();

    return { tabId, url: tab.url, title: tab.title, status: tab.status };
  }

  private async getHtml(params: Record<string, unknown>): Promise<string> {
    const tab = this.getTab(params);
    if (params.selector) {
      throw new Error("selector is not supported in http-fetch runtime");
    }

    const clean = params.clean !== false;
    return clean ? cleanHtml(tab.html) : tab.html;
  }

  private async getText(params: Record<string, unknown>): Promise<string> {
    const tab = this.getTab(params);
    if (params.selector) {
      throw new Error("selector is not supported in http-fetch runtime");
    }
    return htmlToText(tab.html);
  }

  private async getContentSummary(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const tab = this.getTab(params);
    const maxHeadings = Number(params.maxHeadings ?? 20);
    const maxLinks = Number(params.maxLinks ?? 20);
    const maxTextLength = Number(params.maxTextLength ?? 4000);
    const html = cleanHtml(tab.html);
    const text = htmlToText(html).slice(0, maxTextLength);
    const headings = [...html.matchAll(/<h([1-6])[^>]*>([\s\S]*?)<\/h\1>/gi)]
      .slice(0, maxHeadings)
      .map((match) => ({
        level: `h${match[1]}`,
        text: htmlToText(match[2]).slice(0, 200),
      }))
      .filter((item) => item.text);
    const links = [
      ...html.matchAll(/<a[^>]*href=(["'])(.*?)\1[^>]*>([\s\S]*?)<\/a>/gi),
    ]
      .slice(0, maxLinks)
      .map((match) => ({
        href: match[2],
        text: htmlToText(match[3]).slice(0, 160),
      }))
      .filter((item) => item.text || item.href);

    return {
      url: tab.url,
      title: tab.title,
      selector: null,
      headings,
      links,
      forms: [],
      text,
    };
  }

  private async getMetrics(
    params: Record<string, unknown>
  ): Promise<Record<string, unknown>> {
    const tab = this.getTab(params);
    return {
      url: tab.url,
      title: tab.title,
      status: tab.status ?? null,
      contentType: tab.contentType ?? null,
      fetchedAt: tab.fetchedAt ?? null,
      htmlBytes: Buffer.byteLength(tab.html, "utf8"),
      textBytes: Buffer.byteLength(htmlToText(tab.html), "utf8"),
    };
  }

  private async getCookies(url: string): Promise<CookieEntry[]> {
    const normalized = new URL(this.normalizeUrl(url));
    return this.cookiesForUrl(normalized);
  }

  private async setCookie(params: Record<string, unknown>): Promise<null> {
    const normalized = new URL(this.normalizeUrl(String(params.url ?? "")));
    const cookie: CookieEntry = {
      name: String(params.name ?? ""),
      value: String(params.value ?? ""),
      domain: String(params.domain ?? normalized.hostname),
      path: String(params.path ?? "/"),
      secure: params.secure === true,
      httpOnly: params.httpOnly === true,
      expirationDate:
        typeof params.expirationDate === "number"
          ? params.expirationDate
          : undefined,
    };
    this.upsertCookie(cookie);
    return null;
  }

  private async deleteCookie(params: Record<string, unknown>): Promise<null> {
    const normalized = new URL(this.normalizeUrl(String(params.url ?? "")));
    const domain = normalized.hostname;
    const cookies = this.cookieJar.get(domain) ?? [];
    this.cookieJar.set(
      domain,
      cookies.filter((cookie) => cookie.name !== String(params.name ?? ""))
    );
    return null;
  }

  private async getStorage(
    params: Record<string, unknown>
  ): Promise<Record<string, string> | string | null> {
    const tab = this.getTab(params);
    const store =
      params.type === "session" ? tab.sessionStorage : tab.localStorage;
    if (typeof params.key === "string") {
      return store.get(params.key) ?? null;
    }
    return Object.fromEntries(store.entries());
  }

  private async setStorage(params: Record<string, unknown>): Promise<null> {
    const tab = this.getTab(params);
    const store =
      params.type === "session" ? tab.sessionStorage : tab.localStorage;
    store.set(String(params.key ?? ""), String(params.value ?? ""));
    return null;
  }

  private async clearStorage(params: Record<string, unknown>): Promise<null> {
    const tab = this.getTab(params);
    const store =
      params.type === "session" ? tab.sessionStorage : tab.localStorage;
    store.clear();
    return null;
  }

  private buildHeaders(url: string): HeadersInit {
    const normalized = new URL(url);
    const cookies = this.cookiesForUrl(normalized);
    if (cookies.length === 0) {
      return {};
    }
    return {
      Cookie: cookies
        .map((cookie) => `${cookie.name}=${cookie.value}`)
        .join("; "),
    };
  }

  private storeSetCookies(url: string, response: Response): void {
    const setCookies =
      typeof response.headers.getSetCookie === "function"
        ? response.headers.getSetCookie()
        : response.headers.get("set-cookie")
        ? [response.headers.get("set-cookie")!]
        : [];

    for (const header of setCookies) {
      const parsed = parseSetCookie(url, header);
      if (parsed) {
        this.upsertCookie(parsed);
      }
    }
  }

  private upsertCookie(cookie: CookieEntry): void {
    const key = cookie.domain;
    const existing = this.cookieJar.get(key) ?? [];
    const filtered = existing.filter(
      (entry) =>
        !(
          entry.name === cookie.name &&
          entry.path === cookie.path &&
          entry.domain === cookie.domain
        )
    );
    filtered.push(cookie);
    this.cookieJar.set(key, filtered);
  }

  private cookiesForUrl(url: URL): CookieEntry[] {
    const now = Date.now() / 1000;
    return [...this.cookieJar.entries()]
      .filter(([domain]) => domainMatches(url.hostname, domain))
      .flatMap(([, cookies]) =>
        cookies.filter((cookie) => {
          if (cookie.expirationDate && cookie.expirationDate < now) {
            return false;
          }
          if (cookie.secure && url.protocol !== "https:") {
            return false;
          }
          return url.pathname.startsWith(cookie.path);
        })
      );
  }

  private getTab(params: Record<string, unknown>): FetchTab {
    const tabId = this.resolveTabId(params);
    const tab = this.tabs.get(tabId);
    if (!tab) {
      throw new Error(`Tab not found: ${tabId}`);
    }
    return tab;
  }

  private resolveTabId(params: Record<string, unknown>): number {
    if (typeof params.tabId === "number" && Number.isInteger(params.tabId)) {
      return params.tabId;
    }
    if (this.activeTabId !== null) {
      return this.activeTabId;
    }
    throw new Error("No active tab");
  }

  private requireTabId(params: Record<string, unknown>): number {
    if (typeof params.tabId !== "number" || !Number.isInteger(params.tabId)) {
      throw new Error("tabId is required");
    }
    return params.tabId;
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

  private async unsupported(message: string): Promise<never> {
    throw new Error(message);
  }
}
