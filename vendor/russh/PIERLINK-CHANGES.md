<!-- Pierlink 修改：记录流量控制、批量补窗、兼容宽限和关闭生命周期的修改。 -->
# Pierlink 对 russh 0.64.1 的修改

源码取自 crates.io 上的 russh 0.64.1，沿用 Apache-2.0，原始 LICENSE 保持不变。以下列出全部修改；修改过的 Rust 文件首行均有 Pierlink 说明。

## 修改文件与行为

| 文件 | 修改原因与内容 |
|---|---|
| `src/channels/receive.rs`（新增） | 统一服务端有界接收器和客户端窗口约束接收器；新增共享消费凭据 `ChannelConsumption`，累计未消费字节和待归还额度，单个连接共享一个唤醒通知；消费累计跨过 `max(window / 2, max_packet)` 才通知，提取时同样检查阈值；重复确认在调试构建中触发断言。 |
| `src/channels/channel_ref.rs` | 服务端保留有界投递；客户端使用不会等待应用的投递队列，数据字节数由 SSH 窗口限制。释放通道时发出关闭通知，唤醒等待窗口的写入器。保留 `send`、`try_send`，不再 Deref 到有界 Sender。 |
| `src/channels/mod.rs` | `ChannelReadHalf::wait` 在取出数据时归还整条消息的额度；新增 `wait_unconsumed`、`consumption` 供额外缓冲延迟确认。读取句柄释放时归还已丢弃数据；发送窗口新增关闭通知与增量调整，避免覆盖仍在出站队列里的预留额度。新增 `ChannelWriteHalf::closed` 和 `Channel::write_handle`，供独立关闭句柄使用。 |
| `src/channels/io/rx.rs` | AsyncRead 每次只确认实际返回给调用方的字节；明确丢弃的数据也归还额度；Close 与 EOF 都能结束读取，空缓冲立即返回，部分读取器释放时归还丢弃的余量。 |
| `src/channels/io/tx.rs` | 等窗口或等出站队列时同时监听关闭；关闭后当前及后续写入均返回 BrokenPipe，不依赖整条连接的 Sender 被释放。 |
| `src/client/mod.rs` | 客户端主动打开的所有通道改用窗口约束队列；事件循环仅给累计消费达到阈值的通道发送 WINDOW_ADJUST。移除“任一通道有 pending_data 就停收全部出站消息”的全局门槛，复用原有逐通道积压队列；单次最多批量取 65 条普通出站消息，随后重新参与网络/保活/换钥调度。本地关闭立即丢弃对应通道积压并移除通道。收到对端 DISCONNECT 后立即退出，不再排空积压，保留对端断开原因并释放等待写入的应用；部分传输写入取得进展时清零保活失败计数，避免慢速上行误报。仅 Handler 接收、或读句柄已释放时，回调返回视为消费，维持回调用法。等认证回复（none、密码、公钥、证书、agent 签名、GSSAPI）时会话任务已经结束，返回 `RecvError` / `SendError`，不再伪装成“没有剩余方法”的认证失败（键盘交互原本就这样）；服务器真发来的失败照旧返回失败，即使剩余方法为空。 |
| `src/client/session.rs` | 把取消关闭集中在客户端 Session::close，先丢弃该通道积压再发送 Close，并释放通道引用；Handler 主动关闭和 Channel 消息关闭共用此逻辑。 |
| `src/client/encrypted.rs` | DATA 与 EXTENDED_DATA 共用该通道窗口；接收时仅扣窗口，不提前补窗；越窗采用饱和扣减并累计超出字节，超过配置窗口的十分之一才断开；超过声明包长只记录日志，忽略空载荷；投递不等待消费者。服务器主动打开的通道同样适用。远端 Close 清空该通道积压后立即应答，释放引用唤醒写入器。对端补窗以增量更新本地可写额度，扣除低层未预留接口已经重放的积压；客户端只经共享额度和 Handler 报告补窗，不向无界接收队列追加 `WindowAdjusted`。 |
| `src/lib_inner.rs` | 对外导出 `ChannelConsumption`；移除串行等待写入的 `flush_or_timeout`，写入期限改由连接事件循环统一监听。 |
| `src/server/session.rs` | 适配接收器类型与默认消费凭据；传输写入与入站读取共同参与事件循环调度，套接字写满时仍能处理对端数据、补窗与换钥。密文未写完时暂停提取应用发送消息，保留有界队列背压；主动断开前继续发送已排队的报文。部分传输写入取得进展时清零保活失败计数；完全没有读写进展时仍按保活期限退出。 |
| `src/sshbuffer.rs` | 为密文写入保存已发送偏移及待刷新状态，取消写入后重试不会重复或遗漏字节；重试时只在已发送前缀至少占缓冲一半时回收，全部写完则清空，避免频繁取消时反复搬移长缓冲；保存并提取实际写入进展，取消 future 或回收前缀不丢失存活依据。零字节写入立即报错，底层传输错误继续向上传递。 |
| `src/sshbuffer_write_tests.rs`（新增） | 覆盖部分写入后取消并追加报文、半缓冲回收阈值、进度标记提取、刷新挂起后恢复、零写入及传输失败，约束密文的顺序与完整性。 |
| `PIERLINK-CHANGES.md` | 本修改记录。 |

客户端 `src/client/mod.rs` 同样将传输写入放入事件循环，与读取、保活、超时共同调度；写满时暂停提取应用发送消息，但继续处理入站数据和换钥。此前整批写入期间停止读取，双方发送缓冲同时耗尽时可能互相等待，内存双工流和 TCP 都存在该风险。客户端和服务端主动断开后继续排空已有报文，不再发起保活或自动换钥；收到对端断开时立即退出。保活计数同时观察入站报文和已写出的密文字节，写入挂起但仍有进展时不误报，只有排队而没有实际写出时仍累计超时。

没有修改算法、密钥交换实现、协议编码或依赖。补丁涉及的 Rust 文件完整列于上表，便于升级时逐处重放。与上游无关的格式差异（`extended_data_bytes` 签名、导入顺序、`io/tx.rs` 的英文错误原文）保持上游原样，没有整体格式化。

## 窗口与兼容约定

- 正常对端的配置窗口是**单通道未消费数据的逻辑字节上限**。越窗兼容宽限累计最多增加该通道配置窗口的 10%（向下取整），超出即断开；额度累计不因补窗而清零。stdout、stderr、russh 接收队列及引擎输出队列共同使用这份额度；队列节点、协议报文、加密/TCP 缓冲及部分读取后仍被引用的字节块不计入这个统计，因此不是 RSS 的硬上限。补窗按半窗和最大包长两者较大值攒批，通道关闭或读句柄释放不强制补发不足阈值的尾数；通道保持打开时，读取器释放归还的额度与后续 Handler 消费仍按同一阈值补窗。stdout 和 stderr 必须都读，不需要的流可丢弃；只读 stdout 时 stderr 占满共享窗口也会使 stdout 停发。
- 客户端固定采用 `Config::window_size`。`Config::channel_buffer_size` 和 `Handler::adjust_window` 为源码兼容保留，但不再决定客户端接收缓冲，也不再调用客户端的 adjust_window 回调。服务端行为不变。
- `wait()` 取走消息即表示消费。需要继续缓存消息的调用方必须改用 `wait_unconsumed()`，保存 `consumption()` 返回的凭据，并在最终读取或明确丢弃后调用 `consume()`；同一字节不能重复确认。
- 常规 Channel 写入器按对端窗口预留额度，写满后等待。低层 `Handle::data` / `Session::data` 仍是直接排队接口；引擎不使用它们进行大流量写入。同一通道不得混用这两类路径：`increase(amount, replayed)` 的重放扣减只适用于未预留的低层积压，混用会重复扣减写入器预留额度；常规写入器在换钥期间的消息留在出站队列，不形成等窗口积压。低层积压隔离有单独回归测试。
- 其他通道的 EOF 和全部通道的 Close 不受积压阻塞。同一通道 EOF 仍排在该通道此前的数据之后，保证字节顺序；Close 是取消，立即放弃该通道待发数据。密钥交换期间继续遵守 SSH 对非换钥报文的限制。
- Handler 回调仍在连接任务内执行。回调本身须及时返回；本补丁消除的是库内等待通道消费者造成的阻塞。

## 引擎接入

`crates/ssh-engine/src/channel.rs` 不再通过固定 64 KiB 管道提前确认消费，改为共享通道窗口的 stdout/stderr 队列，并公开 `Session::buffered_bytes()`。

`ConnectionOptions::window_size` 默认 256 KiB；批量连接调用方传 `2 * 1024 * 1024`。0 窗口拒绝，最大报文取 32 KiB 与窗口的较小值。

输出读取、转发读取、流式写入及等待退出使用可取消的无限等待。连接断开会广播取消；`Session::closer()`、`Tunnel::closer()` 返回可克隆的 `ChannelCloser`，可从其他线程关闭正在阻塞的读、写和退出等待。关闭句柄使用连接弱引用，单独保存它不会延长连接或运行时寿命，连接已释放时 `close()` 返回 `Disconnected`。`Session::drop` 和显式通道关闭均不以请求超时取消 Close 入队，换钥或共享队列占满期间继续等待，事件循环退出后发送会立即失败。连接、认证、打开通道、全局请求和完整 SFTP INIT/VERSION 握手仍保留期限。

## 相关测试（均在 crates/ssh-engine）

| 场景 | 测试名与断言 |
|---|---|
| 1：A 不读 60 秒，B 往返与 ping 小于 100 ms，保活和换钥正常，恢复后内容完整 | `local_unread_channel_60s_isolated_bounded_complete`：普通用户 sshd，ClientAliveInterval=1 / CountMax=3、RekeyLimit=64K；A 接收 8 MiB，窗口 2 MiB，60 秒后逐字节核对。内存对应 `memory_unread_channel_isolated_bounded_and_complete_with_rekeys`，同时测试 256 KiB / 2 MiB、stdout/stderr 共用窗口。 |
| 2：A 远端 60 秒不读，8 MiB 写入阻塞，B/控制消息正常，远端退出后立即报错 | `local_blocked_upload_60s_other_channel_and_remote_close`：A 执行 `printf ready; sleep 60`，进程退出后写入及再次写入均报错，B 继续回显。内存对应 `memory_blocked_writer_errors_on_remote_close_while_other_channel_works`。`memory_raw_pending_data_does_not_block_controls_and_is_dropped_on_close` 直接制造 russh pending_data，验证 B、EOF、ping、远程转发与撤销。 |
| 3：不读通道缓冲不超过窗口 | 上述不读测试通过 `Session::buffered_bytes()` 持续检查，读取完为 0，并核对全部数据。 |
| 4：读/等待不超时，关闭可取消，请求仍超时，保留原有功能 | `memory_idle_reads_tunnel_and_exit_wait_outlive_request_timeout`、`memory_close_handles_interrupt_pending_reads_tunnels_and_exit_wait`、`memory_disconnect_cancels_reads_tunnels_and_exit_wait`、`memory_local_close_cancels_output_and_wait_without_closing_connection`、`memory_sftp_handshake_retains_request_timeout_and_releases_channel`、`memory_handler_only_receiver_keeps_consuming_after_window_exhaustion`，以及既有引擎测试。 |

## 认证拒绝说明

- `tests/auth_partial_success.rs`：修正两处过时注释，说明连接断开返回 `RecvError`，真实拒绝才携带剩余认证方法，避免把空方法集合误当成断开标志。
