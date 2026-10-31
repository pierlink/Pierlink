/// <reference types="node" />
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { DEFAULT_BYPASS } from "./proxyApi";

describe("代理默认名单", () => {
  it("和后端 core 的 DEFAULT_BYPASS 一致", () => {
    const src = readFileSync("crates/core/src/proxy/mod.rs", "utf8");
    const block = /pub const DEFAULT_BYPASS: &\[&str\] = &\[([\s\S]*?)\];/.exec(src)?.[1] ?? "";
    expect([...block.matchAll(/"([^"]*)"/g)].map((m) => m[1])).toEqual(DEFAULT_BYPASS);
  });
});
