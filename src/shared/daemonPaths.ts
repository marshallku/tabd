import { homedir } from "node:os";

export interface DaemonPaths {
  socketPath: string;
  pidPath: string;
}

export function getDaemonPaths(): DaemonPaths {
  const runtimeDir = process.env.XDG_RUNTIME_DIR;
  const base = runtimeDir
    ? `${runtimeDir}/ai-browser`
    : `${homedir()}/.cache/ai-browser`;
  return {
    socketPath: `${base}/daemon.sock`,
    pidPath: `${base}/daemon.pid`,
  };
}
