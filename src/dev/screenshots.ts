import type { OpenSpec } from "../features/terminal/types";

/** 只在浏览器预览里、地址带 ?shots 时启用：给 README 截图用的示例终端内容 */
export const screenshotMode = import.meta.env.DEV && typeof location !== "undefined" && new URLSearchParams(location.search).has("shots");

const SERVER_PROMPT = "\x1b[1;32mdev@gpu-dev\x1b[0m:\x1b[1;34m~/projects/llm-train\x1b[0m$ ";
const LOCAL_PROMPT = "PS C:\\Users\\you\\projects\\web-app> ";

/** 截图用的提示符 */
export function samplePrompt(spec: OpenSpec): string {
  return spec.kind === "local" ? LOCAL_PROMPT : SERVER_PROMPT;
}

/** 截图用的终端开场内容（不含最后的提示符） */
export function sampleScreen(spec: OpenSpec): string {
  if (spec.kind === "local") {
    return [
      `${LOCAL_PROMPT}npm run build`,
      "",
      "> web-app@1.4.0 build",
      "> vite build",
      "",
      "\x1b[32m✓\x1b[0m 412 modules transformed.",
      "dist/index.html                  0.62 kB",
      "dist/assets/index-3f9a1c.css    18.40 kB │ gzip:  4.12 kB",
      "dist/assets/index-b72e0d.js    148.20 kB │ gzip: 47.91 kB",
      "\x1b[32m✓ built in 2.31s\x1b[0m",
    ].join("\r\n");
  }
  return [
    `${SERVER_PROMPT}git status -sb`,
    "\x1b[32m## main...origin/main\x1b[0m",
    " \x1b[31mM\x1b[0m train.py",
    "\x1b[31m??\x1b[0m configs/run-0042.yaml",
    `${SERVER_PROMPT}python train.py --config configs/run-0042.yaml`,
    "loading dataset  shards=64  samples=12.8M",
    "epoch 3/20  step 1100/4000  loss 1.871  lr 2.0e-4  14.1 it/s",
    "epoch 3/20  step 1200/4000  loss 1.842  lr 2.0e-4  14.2 it/s",
    "epoch 3/20  step 1300/4000  loss 1.817  lr 2.0e-4  14.3 it/s",
    "epoch 3/20  step 1400/4000  loss 1.798  lr 2.0e-4  14.2 it/s",
    "checkpoint saved  ckpt/run-0042/step-1400.pt",
    "^C",
  ].join("\r\n");
}
