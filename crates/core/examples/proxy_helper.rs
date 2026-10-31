//! 代理集成测试的独立帮手：cargo build -p pierlink-core --example proxy_helper。
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let result = if args.len() == 4 && args[1] == "--proxy-connect" {
        args[3]
            .parse::<u16>()
            .ok()
            .map(|port| pierlink_core::proxy::pipe_stdio_from_env(&args[2], port))
    } else {
        None
    };
    std::process::exit(if matches!(result, Some(Ok(()))) { 0 } else { 1 });
}
