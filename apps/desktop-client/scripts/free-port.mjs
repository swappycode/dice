// Best-effort: free a TCP port held by an orphaned dev server before vite's
// strictPort claim, so repeated `just client` / `npm run tauri dev` runs never
// die with "Port 1420 is already in use". No dependencies; never blocks the
// dev loop (any failure is swallowed).
import { execSync } from "node:child_process";

const port = String(process.argv[2] ?? "1420");

function pidsOnWindows() {
  const out = execSync("netstat -ano -p tcp", { encoding: "utf8" });
  const pids = new Set();
  for (const line of out.split(/\r?\n/)) {
    const m = line.match(/:(\d+)\s+\S+\s+LISTENING\s+(\d+)/i);
    if (m && m[1] === port) pids.add(m[2]);
  }
  return [...pids];
}

function pidsOnUnix() {
  try {
    return execSync(`lsof -ti tcp:${port}`, { encoding: "utf8" })
      .split(/\s+/)
      .filter(Boolean);
  } catch {
    return []; // lsof exits non-zero when nothing holds the port
  }
}

try {
  const win = process.platform === "win32";
  const pids = win ? pidsOnWindows() : pidsOnUnix();
  for (const pid of pids) {
    try {
      execSync(win ? `taskkill /PID ${pid} /F` : `kill -9 ${pid}`, { stdio: "ignore" });
      console.log(`free-port: released :${port} (pid ${pid})`);
    } catch {
      /* already gone */
    }
  }
} catch {
  /* never block the dev loop on cleanup */
}
