import { describe, expect, it } from "vitest";
import { isDangerous, LineBuffer, looksLikePasswordPrompt } from "./broadcast";
// 和后端 AI 接入共用的测试向量（Rust 的 safety.rs 用同一个文件）
import vectors from "../../../crates/core/src/mcp/safety_vectors.json";

describe("和后端共用的判断向量", () => {
  it.each(vectors.dangerous.map((v) => [v.line, v.expected] as const))("危险命令 %j → %s", (line, expected) => {
    expect(isDangerous(line)).toBe(expected);
  });
  it.each(vectors.password_prompt.map((v) => [v.tail, v.expected] as const))("密码提示 %j → %s", (tail, expected) => {
    expect(looksLikePasswordPrompt(tail)).toBe(expected);
  });
});

describe("危险命令", () => {
  it.each([
    "rm -rf /tmp/x", "sudo rm -r build", "rm -f a.txt", "rm --recursive x", "cd /; rm -fr *", "make && sudo reboot", "(rm -rf x)",
    "reboot", "sudo shutdown -h now", "poweroff", "init 0", "mkfs.ext4 /dev/sdb1",
    "dd if=/dev/zero of=/dev/sda bs=1M", "chmod -R 777 /", "echo x > /dev/sda",
    ":(){ :|:& };:", "git push -f origin main", "git push --force", "git reset --hard HEAD~1",
    "kubectl delete pod x", "DROP TABLE users;", "drop database prod", "truncate table logs",
  ])("命中：%s", (line) => expect(isDangerous(line)).toBe(true));

  it.each([
    "", "ls -la", "rm a.txt", "echo rm -rf", "git push origin main", "git reset HEAD", "chmod -R 755 ./dist",
    "kubectl get pods", "grep reboot log.txt", "cat /dev/sda.txt",
  ])("不命中：%s", (line) => expect(isDangerous(line)).toBe(false));
});

describe("密码提示", () => {
  it.each([
    "[sudo] password for alice: ", "Password: ", "alice@host's password: ", "Enter passphrase for key '/home/a/.ssh/id_ed25519': ",
    "请输入密码：", "Verification code: ", "\x1b[1mPassword:\x1b[0m ", "some output\r\nPassword: ",
  ])("识别：%j", (tail) => expect(looksLikePasswordPrompt(tail)).toBe(true));

  it.each([
    "alice@host:~$ ", "PS C:\\Users\\a> ", "password updated successfully\r\nalice@host:~$ ", "", "Changing password for alice.\r\n$ ",
  ])("不是：%j", (tail) => expect(looksLikePasswordPrompt(tail)).toBe(false));
});

describe("密码提示与输入回显", () => {
  it("最后一行只是正在输入的命令的回显，不算提示", () => {
    expect(looksLikePasswordPrompt("alice@host:~$ echo password:", "echo password:")).toBe(false);
    expect(looksLikePasswordPrompt("Password: ", "")).toBe(true);
    expect(looksLikePasswordPrompt("[sudo] password for alice: ", "ls")).toBe(true);
  });
});

describe("按键行缓冲", () => {
  it("逐键输入、回车交出整行", () => {
    const b = new LineBuffer();
    expect(b.feed("l")).toEqual([]);
    expect(b.feed("s -la")).toEqual([]);
    expect(b.current).toBe("ls -la");
    expect(b.feed("\r")).toEqual(["ls -la"]);
    expect(b.current).toBe("");
  });

  it("退格、Ctrl+U、Ctrl+W、Ctrl+C", () => {
    const b = new LineBuffer();
    b.feed("rm -rx\x7ff");
    expect(b.current).toBe("rm -rf");
    b.feed(" dir\x17");
    expect(b.current).toBe("rm -rf ");
    b.feed("\x15echo hi");
    expect(b.current).toBe("echo hi");
    b.feed("\x03");
    expect(b.current).toBe("");
  });

  it("方向键等控制序列忽略，括号粘贴的内容照常计入，\\r\\n 只算一次回车", () => {
    const b = new LineBuffer();
    expect(b.feed("ab\x1b[D\x1b[Cc\x1bOA")).toEqual([]);
    expect(b.current).toBe("abc");
    b.reset();
    expect(b.feed("\x1b[200~line1\r\nline2\x1b[201~\r")).toEqual(["line1", "line2"]);
  });

  it("中文和多字节字符退格", () => {
    const b = new LineBuffer();
    b.feed("删除𠀀\x7f");
    expect(b.current).toBe("删除");
  });
});
