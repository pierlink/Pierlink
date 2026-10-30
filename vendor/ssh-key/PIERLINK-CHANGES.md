# Pierlink 对 ssh-key 0.7.0-rc.11 的修改

源码取自 crates.io 上的 ssh-key 0.7.0-rc.11，沿用 Apache-2.0 OR MIT，原始 LICENSE-APACHE、LICENSE-MIT 保持不变。以下列出全部修改；修改过的 Rust 文件首行有 Pierlink 说明。

| 文件 | 修改原因与内容 |
|---|---|
| `src/private/ecdsa.rs` | ECDSA 私钥按 `mpint` 编码，私钥数字开头是 0 时 OpenSSH 会省掉这些 0（P-256 约每 256 个密钥有一个只有 31 字节）。上游要求至少 32 字节，这类密钥读不出来。改为接受 1 到曲线尺寸之间的任意长度，开头补 0，和 OpenSSH 的读取一致。 |
| `tests/`、`Cargo.toml` 里的 `[[test]]` | 删除上游的测试和测试数据（其中有示例私钥，Pierlink 不跑这些测试）。 |
| `src/lib.rs`、`src/private.rs` | 只改文档注释：示例私钥的内容换成占位文字（避免被当成泄露的密钥），代码不变。 |
| `PIERLINK-CHANGES.md` | 本修改记录。 |

除此之外没有修改其他文件、算法或依赖。上游修复后改回 crates.io 版本即可。
