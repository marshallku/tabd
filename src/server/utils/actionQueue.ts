// ActionQueue — serializer for browser actions in a shared daemon.
//
// The daemon hosts a single Chromium and exposes it to N concurrent clients
// (MCP sessions, CLI invocations, AI agents). Without coordination, two
// clients can race on the same tab — for example one navigates while
// another waits for a selector against the URL that just changed.
//
// The queue assigns every action a scope and chains actions in the same
// scope one after another. Two scopes execute concurrently with each other,
// but a "global" scope (typically tab-structural operations like open/close)
// holds a lock that blocks any other scope until it completes. The
// semantics are designed so a single client never pays a concurrency cost
// (its own actions are already serial), while N clients can run in parallel
// against different tabs without observing each other's mid-action state.
//
// The queue does not understand actions — it only serializes function
// invocations by scope. Cancellation, timeout, and drain are handled by
// the caller; this module just guarantees ordering.

export type Scope = string;
export const GLOBAL: Scope = "__global__";

export class ActionQueue {
  // Per-scope chain: each new action chains onto the previous one's
  // settlement so failures do not break the chain order.
  private readonly chains = new Map<Scope, Promise<unknown>>();
  // Lock held while a global scope action is running. Per-scope actions
  // await this lock at enqueue resolution time, before their own work.
  private globalLock: Promise<unknown> = Promise.resolve();

  enqueue<T>(scope: Scope, fn: () => Promise<T>): Promise<T> {
    if (scope === GLOBAL) {
      // Global serializes against all current per-scope chains AND prior
      // globals. Once started, subsequent per-scope work waits on this
      // global via the globalLock.
      const waitAllScopes = Promise.allSettled([...this.chains.values()]);
      const waitGlobal = this.globalLock;
      const gate = Promise.allSettled([waitAllScopes, waitGlobal]);
      const next = gate.then(fn);
      // Other globals chain after this one
      this.globalLock = next.catch(() => undefined);
      // Tab queues will pick up next via globalLock; do not register in
      // chains map so the global does not also become a per-scope task.
      return next;
    }

    const prev = this.chains.get(scope) ?? Promise.resolve();
    const gate = Promise.allSettled([prev, this.globalLock]);
    const next = gate.then(fn);
    // Settle-only chain pointer; the returned promise carries real rejection
    // to the caller while subsequent enqueues see a no-throw predecessor.
    const settled = next.catch(() => undefined);
    this.chains.set(scope, settled);
    // Best-effort GC: when this scope's chain becomes idle, drop the entry
    // to keep the map small for long-lived daemons with many transient tabs.
    void settled.then(() => {
      if (this.chains.get(scope) === settled) {
        this.chains.delete(scope);
      }
    });
    return next;
  }

  /** Wait for every in-flight action (per-scope and global) to settle. */
  async drain(): Promise<void> {
    while (true) {
      const pending = [this.globalLock, ...this.chains.values()];
      if (pending.length === 0) return;
      await Promise.allSettled(pending);
      // New work could have been enqueued while we awaited; loop until idle.
      if (
        this.chains.size === 0 &&
        // globalLock is replaced by Promise.resolve() once nothing references it,
        // so checking length+settled is enough. We force one more allSettled tick.
        (await Promise.race([
          Promise.allSettled([this.globalLock]).then(() => "settled" as const),
          new Promise<"unsettled">((resolve) =>
            setImmediate(() => resolve("unsettled"))
          ),
        ])) === "settled"
      ) {
        return;
      }
    }
  }

  /** Diagnostic snapshot — number of distinct scopes with pending work. */
  inflightScopeCount(): number {
    return this.chains.size;
  }
}
