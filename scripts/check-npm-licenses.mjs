// 检查前端生产依赖的许可证是否在白名单内（随安装包分发的只有生产依赖）。
// 用法：npm run check:licenses
import { execSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const ALLOW = new Set([
  "MIT", "MIT-0", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "0BSD",
  "CC0-1.0", "Unicode-3.0", "MPL-2.0", "OFL-1.1", "BlueOak-1.0.0", "Python-2.0", "CC-BY-4.0",
]);

/** 判断 SPDX 表达式是否允许：OR 任一满足即可，AND 需全部满足。 */
function allowed(expr) {
  const e = String(expr).replace(/[()]/g, " ").trim();
  if (/\sOR\s/.test(e)) return e.split(/\sOR\s/).some((x) => allowed(x));
  if (/\sAND\s/.test(e)) return e.split(/\sAND\s/).every((x) => allowed(x));
  return ALLOW.has(e.trim());
}

const tree = JSON.parse(execSync("npm ls --omit=dev --all --json", { encoding: "utf8" }));
const seen = new Map();
(function walk(deps, base) {
  for (const [name, info] of Object.entries(deps ?? {})) {
    // 未安装的可选依赖（如其他平台的二进制包）没有版本号，跳过
    if (!info.version || info.missing) continue;
    const key = `${name}@${info.version}`;
    if (seen.has(key)) continue;
    const pkgPath = join("node_modules", name, "package.json");
    let license = "未知";
    try {
      const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));
      license = pkg.license ?? (Array.isArray(pkg.licenses) ? pkg.licenses.map((l) => l.type).join(" OR ") : "未知");
    } catch {
      license = "未知";
    }
    seen.set(key, license);
    walk(info.dependencies, base);
  }
})(tree.dependencies);

const bad = [...seen].filter(([, lic]) => !allowed(lic));
for (const [k, lic] of seen) console.log(`许可证: 包=${k}, 许可证=${lic}`);
if (bad.length) {
  console.error(`\n发现不在白名单内的许可证: 数量=${bad.length}`);
  for (const [k, lic] of bad) console.error(`  不允许: 包=${k}, 许可证=${lic}`);
  process.exit(1);
}
console.log(`\n许可证检查通过: 生产依赖数=${seen.size}`);
