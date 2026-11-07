import { describe, expect, it } from "vitest";
import { isDenied, serverFixCommand } from "./permissions";

describe("isDenied", () => {
  it("认出 Windows 和 Linux 的没有权限", () => {
    expect(isDenied("opening temp file: open C:\\a\\b.tmp: Access is denied.")).toBe(true);
    expect(isDenied("lstat /srv/a: permission denied")).toBe(true);
    expect(isDenied("The process cannot access the file because it is being used by another process.")).toBe(false);
  });
});

describe("serverFixCommand", () => {
  it("取所在文件夹，只留最上层的", () => {
    expect(serverFixCommand("/home/u/proj/", ["a/b/c.txt", "a/d.txt", "x/y.txt"]))
      .toBe(`sudo chown -R "$(id -un)": '/home/u/proj/a' '/home/u/proj/x' && sudo chmod -R u+rwX '/home/u/proj/a' '/home/u/proj/x'`);
  });
  it("同步目录最外层的文件 → 同步目录本身", () => {
    expect(serverFixCommand("/srv/app", ["a/b.txt", "c.txt"]))
      .toBe(`sudo chown -R "$(id -un)": '/srv/app' && sudo chmod -R u+rwX '/srv/app'`);
  });
  it("路径里的单引号转义，不合法的路径跳过", () => {
    expect(serverFixCommand("/srv/it's", ["a/b"])).toBe(`sudo chown -R "$(id -un)": '/srv/it'\\''s/a' && sudo chmod -R u+rwX '/srv/it'\\''s/a'`);
    expect(serverFixCommand("/srv", ["../etc/passwd", "/etc/shadow", ""])).toBe("");
  });
});
