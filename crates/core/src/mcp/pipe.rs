//! Windows 入口：Pierlink 主程序在本机命名管道上提供 MCP 服务；`Pierlink.exe --mcp`（桥接程序）
//! 把 agent 的标准输入输出接到这个管道上。管道只许 SYSTEM 和当前用户连接、拒绝远程连接；
//! 服务端核对连进来的是同一个程序，桥接端核对管道另一头是同一个程序、同一个用户
//! （防别的用户抢先建同名管道截获 agent 的请求）。
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(windows)]
#[path = "pipe/windows.rs"]
mod platform;
#[cfg(windows)]
pub use platform::{pipe_name, run_bridge, serve};

/// 连不上 Pierlink 时回给 agent 的说明
pub const UNAVAILABLE: &str =
    "Pierlink 没有运行：请先打开 Pierlink，并在 Pierlink 顶部的 MCP 页里打开 Windows 入口";

/// 在 agent 的标准输入输出和已连上的管道之间双向转发。任一方向结束（agent 关了标准输入，
/// 或 Pierlink 断开管道）就返回；管道到标准输出每写一块就刷新，保证消息及时送到 agent。
///
/// :param input: agent 写给我们的（标准输入）
/// :param output: 写给 agent 的（标准输出）
/// :param pipe: 已连上并核对过的管道
/// :return: 正常结束返回 Ok；读写出错返回对应的 IO 错误
pub async fn relay<I, O, P>(mut input: I, mut output: O, pipe: P) -> std::io::Result<()>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    P: AsyncRead + AsyncWrite + Unpin,
{
    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);
    let upstream = async {
        tokio::io::copy(&mut input, &mut pipe_write).await?;
        pipe_write.shutdown().await
    };
    let downstream = async {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let count = pipe_read.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            output.write_all(&buffer[..count]).await?;
            output.flush().await?;
        }
    };
    tokio::select! {
        result = upstream => result,
        result = downstream => result,
    }
}

/// 连不上 Pierlink 时：对 agent 发来的每个请求回同一个 JSON-RPC 错误，通知不回，直到标准输入结束。
/// agent 据此把原因显示给用户，而不是一直等到超时。
///
/// :param input: agent 的标准输入
/// :param output: agent 的标准输出
/// :param message: 错误说明
/// :return: 标准输入结束返回 Ok；读写出错返回对应的 IO 错误
pub async fn answer_unavailable<I, O>(input: I, mut output: O, message: &str) -> std::io::Result<()>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id").filter(|id| !id.is_null()).cloned() else {
            continue;
        };
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": message },
        });
        output.write_all(format!("{response}\n").as_bytes()).await?;
        output.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn 双向转发且管道断开时结束() {
        runtime().block_on(async {
            let (agent_in, mut agent_writes) = duplex(1024);
            let (mut agent_reads, agent_out) = duplex(1024);
            let (pipe, mut pierlink) = duplex(1024);
            let task = tokio::spawn(relay(agent_in, agent_out, pipe));
            agent_writes.write_all(b"{\"id\":1}\n").await.unwrap();
            let mut line = String::new();
            let mut from_agent = BufReader::new(&mut pierlink);
            from_agent.read_line(&mut line).await.unwrap();
            assert_eq!(line, "{\"id\":1}\n");
            pierlink.write_all(b"{\"result\":1}\n").await.unwrap();
            let mut answer = String::new();
            BufReader::new(&mut agent_reads)
                .read_line(&mut answer)
                .await
                .unwrap();
            assert_eq!(answer, "{\"result\":1}\n");
            drop(pierlink);
            task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn agent_关闭标准输入时结束() {
        runtime().block_on(async {
            let (agent_in, agent_writes) = duplex(1024);
            let (_agent_reads, agent_out) = duplex(1024);
            let (pipe, _pierlink) = duplex(1024);
            let task = tokio::spawn(relay(agent_in, agent_out, pipe));
            drop(agent_writes);
            task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn 连不上时对请求回错误且不回通知() {
        runtime().block_on(async {
            let (agent_in, mut agent_writes) = duplex(4096);
            let (mut agent_reads, agent_out) = duplex(4096);
            let task = tokio::spawn(answer_unavailable(agent_in, agent_out, UNAVAILABLE));
            agent_writes
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
                      not json\n\
                      {\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"initialize\"}\n\
                      {\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\"}\n",
                )
                .await
                .unwrap();
            drop(agent_writes);
            task.await.unwrap().unwrap();
            let mut text = String::new();
            agent_reads.read_to_string(&mut text).await.unwrap();
            let answers: Vec<serde_json::Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(answers.len(), 2);
            assert_eq!(answers[0]["id"], "a");
            assert_eq!(answers[1]["id"], 7);
            assert_eq!(answers[1]["error"]["message"], UNAVAILABLE);
        });
    }
}
