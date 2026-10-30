import { describe, expect, it } from "vitest";
import { historyScope, isSensitive, nextWord, suggest, type HistoryEntry } from "./predict";

const e = (command: string, cwd: string | null = "/home/a", exit: number | null = 0, at = 0): HistoryEntry => ({ command, cwd, exit, at });

describe("命令预测", () => {
  it("同一目录下用过的优先，其次最近用过的", () => {
    const h = [e("git status", "/srv/x"), e("git stash", "/home/a"), e("git switch main", "/srv/y")];
    expect(suggest(h, "git s", "/home/a")).toBe("git stash");
    expect(suggest(h, "git s", "/srv/x")).toBe("git status");
    expect(suggest(h, "git s", "/elsewhere")).toBe("git switch main");
    expect(suggest(h, "git s", null)).toBe("git switch main");
  });
  it("跳过失败的命令；Ctrl+C、SIGTERM 结束的不算失败", () => {
    const h = [e("npm run dev", "/p", 130), e("npm run build", "/p", 2), e("npm run lint", "/p", 143)];
    expect(suggest(h, "npm run b", "/p")).toBeNull();
    expect(suggest(h, "npm run d", "/p")).toBe("npm run dev");
    expect(suggest(h, "npm run l", "/p")).toBe("npm run lint");
    // 不知道退出码的照常预测
    expect(suggest([e("make", "/p", null)], "ma", "/p")).toBe("make");
  });
  it("输入为空、只有空格、多行，或者和候选一样长时不预测", () => {
    const h = [e("ls -la")];
    expect(suggest(h, "", "/home/a")).toBeNull();
    expect(suggest(h, "  ", "/home/a")).toBeNull();
    expect(suggest(h, "ls\n-", "/home/a")).toBeNull();
    expect(suggest(h, "ls -la", "/home/a")).toBeNull();
    // 区分大小写
    expect(suggest(h, "LS", "/home/a")).toBeNull();
  });
  it("Ctrl+→ 一次补一个词，路径按 / 分段", () => {
    expect(nextWord("eckout main")).toBe("eckout");
    expect(nextWord(" main")).toBe(" main");
    expect(nextWord("src/features/terminal")).toBe("src/");
    expect(nextWord("/etc/hosts")).toBe("/");
    expect(nextWord("x")).toBe("x");
  });
  it("可能带口令、令牌、私钥的命令不记", () => {
    for (const c of [
      "mysql -u root -pS3cret db",
      "export GITHUB_TOKEN=abc",
      "curl -H 'Authorization: Bearer x' https://a",
      "echo $API_KEY",
      "sshpass -p x ssh host",
      "docker login --password x",
      "aws configure set aws_access_key_id AKIAABCDEFGHIJKLMNOP",
      "git clone https://ghp_abcdefghijklmnopqrstuvwxyz0123@github.com/a/b",
      "cat ~/notes/passwords.txt",
      "a\u0007b",
      "x".repeat(1001),
    ]) expect(isSensitive(c), c).toBe(true);
    for (const c of ["git push", "npm run dev", "ls -la", "ssh -p 2222 host", "python train.py --epochs 3", "tokei src"]) {
      // tokei 含 token 的前几个字母但不是 token：照常记
      expect(isSensitive(c), c).toBe(false);
    }
  });
  it("历史按服务器、快速连接、本机 Git Bash 分开；PowerShell 等不预测", () => {
    expect(historyScope({ kind: "server", serverId: "s1" })).toBe("server:s1");
    expect(historyScope({ kind: "quick", host: "Example.COM", port: 22, user: "root" })).toBe("quick:root@example.com:22");
    expect(historyScope({ kind: "quick", host: "h", port: 2222, user: null })).toBe("quick:@h:2222");
    expect(historyScope({ kind: "local", shellId: "c:\\program files\\git\\bin\\bash.exe" })).toBe("local:bash");
    expect(historyScope({ kind: "local", shellId: "c:\\program files\\powershell\\7\\pwsh.exe" })).toBeNull();
    expect(historyScope({ kind: "local", shellId: "c:\\windows\\system32\\wsl.exe#Ubuntu" })).toBeNull();
  });
});
