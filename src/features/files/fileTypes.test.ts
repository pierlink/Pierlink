import { describe, expect, it } from "vitest";
import { isBinaryName } from "./fileTypes";

describe("按扩展名判断二进制文件", () => {
  it("图片、压缩包、模型文件不可编辑；代码、配置、没有扩展名的可以", () => {
    for (const n of ["a.PNG", "photo.jpeg", "x.tar.gz", "model.safetensors", "app.exe", "report.pdf"]) expect(isBinaryName(n)).toBe(true);
    for (const n of ["main.py", "nginx.conf", "Makefile", ".bashrc", "icon.svg", "data.csv", "a.", "README", "app.ts", "model.obj"]) expect(isBinaryName(n)).toBe(false);
  });
});
