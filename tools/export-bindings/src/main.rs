//! 生成前端类型文件 `src/bindings/commands.ts`（CI 和开发时使用，在仓库根目录运行）。
//! 用独立程序而不是测试：Windows 上 Tauri 的测试程序缺少应用清单，无法启动。
//! 放在单独的 crate 里：放在 src-tauri 下会被 Tauri 当成应用的一部分打进安装包。

fn main() {
    match pierlink_lib::export_bindings() {
        Ok(()) => println!("前端类型已生成: 路径=src/bindings/commands.ts"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
