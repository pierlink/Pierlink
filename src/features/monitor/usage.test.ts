import { describe, expect, it } from "vitest";
import { diskUsage } from "./usage";

describe("磁盘用量", () => {
  it("和 df 一样：已用 / (已用 + 可用)，不把 root 预留算成已用", () => {
    // 空的 1 TB 盘：df 显示 1%，以前按“总量 − 可用”会显示 5%
    const gb = 2 ** 30;
    const r = diskUsage({ mount: "/", fsType: "ext4", total: 1007 * gb, used: 1.5 * gb, free: 954 * gb });
    expect(r.used).toBe(1.5 * gb);
    expect(Math.ceil(r.percent ?? 0)).toBe(1);
  });

  it("旧版本没有 used 时退回总量减可用", () => {
    const r = diskUsage({ mount: "/", fsType: "ext4", total: 100, used: 0, free: 40 });
    expect(r.used).toBe(60);
    expect(r.percent).toBe(60);
  });

  it("总量为 0 时没有百分比", () => {
    expect(diskUsage({ mount: "/", fsType: "tmpfs", total: 0, used: 0, free: 0 }).percent).toBeNull();
  });
  it("ext4 给 root 预留 5% 时百分比和 df 一样，并报出预留量", () => {
    // 开发机数据盘的真实数字：df 显示 79%（不是 43.9 / 58.8 的 75%）
    const r = diskUsage({ mount: "/data", fsType: "ext4", total: 63090503680, used: 47182852096, free: 12669648896 });
    expect(Math.round(r.percent!)).toBe(79);
    expect(Math.round(r.reserved / 1e9)).toBe(3);
  });

  it("几乎没有预留时不提预留", () => {
    const r = diskUsage({ mount: "/", fsType: "ext4", total: 20617822208, used: 16191098880, free: 4409946112 });
    expect(Math.round(r.percent!)).toBe(79);
    expect(r.reserved).toBe(0);
  });
});
