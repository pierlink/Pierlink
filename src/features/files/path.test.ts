import { describe, expect, it } from "vitest";
import { baseName, crumbs, formatSize, joinPath, parentPath, relativeTo, sameLoc, syncCovering } from "./path";

describe("路径工具", () => {
  it("拼接按路径风格", () => {
    expect(joinPath("/home/me", "a.txt")).toBe("/home/me/a.txt");
    expect(joinPath("/", "etc")).toBe("/etc");
    expect(joinPath("C:\\Users\\me", "a.txt")).toBe("C:\\Users\\me\\a.txt");
    expect(joinPath("C:\\", "Users")).toBe("C:\\Users");
  });
  it("上一级", () => {
    expect(parentPath("/home/me")).toBe("/home");
    expect(parentPath("/home")).toBe("/");
    expect(parentPath("/")).toBeNull();
    expect(parentPath("C:\\Users\\me")).toBe("C:\\Users");
    expect(parentPath("C:\\Users")).toBe("C:\\");
    expect(parentPath("C:\\")).toBeNull();
  });
  it("最后一段和面包屑", () => {
    expect(baseName("/home/me/")).toBe("me");
    expect(baseName("/")).toBe("/");
    expect(crumbs("/home/me").map((c) => c.name)).toEqual(["/", "home", "me"]);
    expect(crumbs("C:\\Users\\me").map((c) => c.path)).toEqual(["C:\\", "C:\\Users", "C:\\Users\\me"]);
    expect(crumbs("C:\\Users\\me")[0].name).toBe("C:");
  });
  it("相对路径：Windows 不区分大小写", () => {
    expect(relativeTo("C:\\Code\\Proj\\src", "c:/code/proj")).toBe("src");
    expect(relativeTo("/srv/proj", "/srv/proj/")).toBe("");
    expect(relativeTo("/srv/project", "/srv/proj")).toBeNull();
  });
  it("同一台机器", () => {
    expect(sameLoc({ kind: "local" }, { kind: "local" })).toBe(true);
    expect(sameLoc({ kind: "server", serverId: "a" }, { kind: "server", serverId: "b" })).toBe(false);
    expect(sameLoc({ kind: "local" }, { kind: "server", serverId: "a" })).toBe(false);
  });
  it("大小显示", () => {
    expect(formatSize(12)).toBe("12 B");
    expect(formatSize(1536)).toBe("1.5 KB");
    expect(formatSize(300 * 1024 * 1024)).toBe("300 MB");
  });
});

describe("同步覆盖判断", () => {
  const pairs = [{ name: "proj", localPath: "D:\\code\\proj", serverId: "s1", remotePath: "~/proj" }];
  const local = (dir: string) => ({ loc: { kind: "local" } as const, dir });
  const srv = (dir: string, serverId = "s1") => ({ loc: { kind: "server", serverId } as const, dir });
  it("对应目录（含子目录）算覆盖，方向无关", () => {
    expect(syncCovering(pairs, local("D:\\code\\proj"), srv("/home/dev/proj"), "/home/dev")).toBe("proj");
    expect(syncCovering(pairs, srv("/home/dev/proj/src"), local("d:/code/proj/src"), "/home/dev")).toBe("proj");
  });
  it("相对位置不同、别的服务器、不知道家目录都不算", () => {
    expect(syncCovering(pairs, local("D:\\code\\proj\\src"), srv("/home/dev/proj"), "/home/dev")).toBeNull();
    expect(syncCovering(pairs, local("D:\\code\\proj"), srv("/home/dev/proj", "s2"), "/home/dev")).toBeNull();
    expect(syncCovering(pairs, local("D:\\code\\proj"), srv("/home/dev/proj"), null)).toBeNull();
  });
});
