import { homedir } from "node:os";

export interface DaemonPaths {
  socketPath: string;
  pidPath: string;
}

export interface DaemonPathOverrides {
  /** Override the socket path entirely. Takes precedence over env. */
  socketPath?: string;
  /** Override the pid file path entirely. Takes precedence over env. */
  pidPath?: string;
  /** Override the base directory (socket + pid land in `${base}/daemon.{sock,pid}`). */
  baseDir?: string;
}

/**
 * Resolve socket/pid paths. Default location is `$XDG_RUNTIME_DIR/ai-browser/`
 * with `~/.cache/ai-browser/` fallback. Callers can pass explicit paths via
 * `overrides` — useful for the `run-once` ephemeral daemon, which needs an
 * isolated socket so it does not collide with a long-running daemon owned
 * by the user.
 *
 * Precedence (per field): overrides > env > default.
 */
export function getDaemonPaths(overrides?: DaemonPathOverrides): DaemonPaths {
  // env override is consulted between explicit overrides and the default,
  // so `run-once` can spawn child processes with AI_BROWSER_BASE_DIR set
  // and have the entire daemon/client stack agree on an isolated socket
  // without threading a path argument through every layer.
  const envBase = process.env.AI_BROWSER_BASE_DIR;
  const baseDir =
    overrides?.baseDir ??
    envBase ??
    (process.env.XDG_RUNTIME_DIR
      ? `${process.env.XDG_RUNTIME_DIR}/ai-browser`
      : `${homedir()}/.cache/ai-browser`);

  return {
    socketPath: overrides?.socketPath ?? `${baseDir}/daemon.sock`,
    pidPath: overrides?.pidPath ?? `${baseDir}/daemon.pid`,
  };
}
