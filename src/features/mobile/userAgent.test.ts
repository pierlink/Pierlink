import { describe, expect, it } from "vitest";
import { deviceSummary } from "./userAgent";

describe("手机说明", () => {
  it("认出常见的系统和浏览器", () => {
    expect(deviceSummary("Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1")).toBe("iPhone · Safari");
    expect(deviceSummary("Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/124.0.6367.88 Mobile/15E148 Safari/604.1")).toBe("iPhone · Chrome");
    expect(deviceSummary("Mozilla/5.0 (Linux; Android 14; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36")).toBe("Android 14 · Chrome");
    expect(deviceSummary("Mozilla/5.0 (iPad; CPU OS 16_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1")).toBe("iPad · Safari");
  });

  it("微信、QQ 浏览器这类套壳浏览器认成它们自己", () => {
    expect(deviceSummary("Mozilla/5.0 (Linux; Android 13; V2227A Build/TP1A.220624.014; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/116.0.0.0 Mobile Safari/537.36 XWEB/1160065 MMWEBSDK/20231202 MicroMessenger/8.0.47.2560(0x28002F30) WeChat/arm64")).toBe("Android 13 · WeChat");
    expect(deviceSummary("Mozilla/5.0 (Linux; U; Android 12; zh-cn; M2012K11AC Build/SKQ1.211006.001) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/100.0.4896.58 Mobile Safari/537.36 XiaoMi/MiuiBrowser/17.6.80")).toBe("Android 12 · Mi Browser");
    expect(deviceSummary("Mozilla/5.0 (Linux; Android 10; HarmonyOS; ELS-AN00) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/99.0.4844.88 HuaweiBrowser/14.0.5.302 Mobile Safari/537.36")).toBe("HarmonyOS · Huawei Browser");
  });

  it("没有或认不出时没有说明", () => {
    expect(deviceSummary(null)).toBeNull();
    expect(deviceSummary("")).toBeNull();
    expect(deviceSummary("curl/8.5.0")).toBeNull();
  });
});
