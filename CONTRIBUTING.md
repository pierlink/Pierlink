# Contributing / 参与贡献

[English](#english) · [简体中文](#简体中文)

## English

Thanks for your interest in Pierlink!

- **Bugs and ideas**: open an [issue](https://github.com/pierlink/Pierlink/issues/new/choose) using the templates. Never paste passwords, private keys or tokens.
- **Security issues**: follow [SECURITY.md](SECURITY.md) instead of opening a public issue.
- **Code**: please open an issue to discuss the change before sending a pull request, so no effort is wasted.

### License of contributions

Pierlink is released under [FSL-1.1-ALv2](LICENSE). By submitting a pull request you confirm that you have the right to submit the contribution, and you grant the maintainer a perpetual, worldwide, non-exclusive, royalty-free, irrevocable license (including under any patents you hold that the contribution necessarily infringes) to use, reproduce, modify, distribute, sublicense and relicense it under FSL-1.1-ALv2, the licenses it converts to, and any other terms, including commercial ones. There is no separate CLA.

### Guidelines

- Commit messages in English, with a one-line summary.
- Keep UI text short, and add every string in both `src/locales/en.json` and `src/locales/zh-CN.json`.
- New dependencies must pass `cargo deny check licenses` and `npm run check:licenses` (no GPL / AGPL / LGPL / SSPL).
- Before submitting, run what CI runs (`.github/workflows/ci.yml`): `npm test`, `cargo fmt --all -- --check`, `cargo clippy -p pierlink-core -p pierlink-ssh-engine --all-targets -- -D warnings`, `cargo test -p pierlink-core` and `cargo test -p pierlink-ssh-engine -- --test-threads=1`. On Windows also run `cargo clippy -p pierlink --all-targets -- -D warnings`.

## 简体中文

感谢你关注 Pierlink！

- **问题和建议**：用模板提 [Issue](https://github.com/pierlink/Pierlink/issues/new/choose)。不要贴出密码、私钥或令牌。
- **安全问题**：按 [SECURITY.md](SECURITY.md) 私下报告，不要公开提 Issue。
- **代码**：发 Pull Request 之前，请先开 Issue 讨论改动，免得白费功夫。

### 贡献的许可

Pierlink 按 [FSL-1.1-ALv2](LICENSE) 发布。提交 Pull Request 即表示你确认自己有权提交这些贡献，并授予维护者永久、全球、非独占、免版税、不可撤销的许可（含你持有的、该贡献必然涉及的专利），可以按 FSL-1.1-ALv2、它转换成的许可证以及任何其他条款（包括商业条款）使用、复制、修改、分发、再许可和重新授权。不需要另外签 CLA。

### 约定

- 提交说明用英文，第一行一句话概括。
- 界面文字要简短，每条文字都同时加到 `src/locales/en.json` 和 `src/locales/zh-CN.json`。
- 新依赖要通过 `cargo deny check licenses` 和 `npm run check:licenses`（不能引入 GPL / AGPL / LGPL / SSPL）。
- 提交前跑一遍 CI 跑的检查（`.github/workflows/ci.yml`）：`npm test`、`cargo fmt --all -- --check`、`cargo clippy -p pierlink-core -p pierlink-ssh-engine --all-targets -- -D warnings`、`cargo test -p pierlink-core`、`cargo test -p pierlink-ssh-engine -- --test-threads=1`；Windows 上再跑 `cargo clippy -p pierlink --all-targets -- -D warnings`。
