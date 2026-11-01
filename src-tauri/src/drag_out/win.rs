//! Windows 的系统拖放：把还没下载的文件作为 CF_HDROP 拖出去，松手放到别的程序上时才下载。
//!
//! - 只依赖 `windows`、`windows-core`、标准库和同目录的 `format`、`job`，可以单独拿出来做 Windows 目标的类型检查；
//! - 拖放在主线程上进行。[`start`] 只设一个线程定时器，拖放在定时器回调里开始：回调由 tao 的消息循环直接调用，
//!   不在它的事件处理函数里，所以拖放和下载期间界面的其他事件（终端输出、下载进度）照常分发；
//! - 松手时（`QueryContinueDrag`）在工作线程里下载，主线程一边等一边处理消息，并显示系统的进度框（它自己有线程）；
//! - 两个 COM 对象都不做自由线程封送（`Agile = false`）：别的套间调用时由 COM 封送回主线程，状态只在主线程上读写。

use super::format::{DROPFILES_SIZE, effect_bytes, hdrop_bytes};
use super::job::{Failure, Fetch, Labels, Outcome, Progress, Request};
use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_DVASPECT, DV_E_FORMATETC, DV_E_TYMED, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED,
    GlobalFree, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, S_OK,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0,
    TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetActiveWindow, VK_ESCAPE};
use windows::Win32::UI::Shell::{
    CFSTR_PREFERREDDROPEFFECT, CLSID_ProgressDialog, DROPFILES, IProgressDialog, PDTIMER_RESET,
    PROGDLG_AUTOTIME, PROGDLG_NOMINIMIZE, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow,
    GetWindowThreadProcessId, KillTimer, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
    PM_QS_INPUT, PM_QS_POSTMESSAGE, PM_QS_SENDMESSAGE, PM_REMOVE, PeekMessageW, PostQuitMessage,
    QS_ALLINPUT, QS_PAINT, SetTimer, TranslateMessage, WM_KEYDOWN, WM_PAINT, WM_QUIT,
    WindowFromPoint,
};
use windows::core::{BOOL, HRESULT, HSTRING, Ref, implement};

// CF_HDROP 的头由 format 按这个大小拼
const _: () = assert!(std::mem::size_of::<DROPFILES>() == DROPFILES_SIZE);

/// 下载期间多久看一次结果、更新一次进度框（毫秒）
const POLL_MS: u32 = 100;
/// 取消后最多再等下载线程多久：网络卡住时它可能迟迟看不到取消，不让拖放一直结束不了
const CANCEL_WAIT: Duration = Duration::from_secs(5);

thread_local! {
    /// 等定时器触发的拖出：定时器 ID → 请求
    static PENDING: RefCell<Vec<(usize, Request)>> = const { RefCell::new(Vec::new()) };
    /// 正在拖出（同时只能有一个：系统拖放本身就是独占的）
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// 开始拖出。在主线程上调用（tauri 的 run_on_main_thread 里）。
///
/// 这里只设一个只用一次的线程定时器，拖放在定时器回调里开始：那时已经回到 tao 的消息循环、不在它的
/// 事件处理函数里。要是直接在这里拖放，整个拖放和下载期间 tao 会把所有事件攒着不发，
/// 终端输出、下载进度、拖回窗口时的高亮都要等拖放结束才一起出来。
///
/// :param request: 这次拖出；结束时（包括没能开始）一定会调用它的 done
pub fn start(request: Request) {
    // SAFETY: 线程定时器（不绑窗口），回调是本模块里的函数
    let id = unsafe { SetTimer(None, 0, 0, Some(on_timer)) };
    if id == 0 {
        (request.done)(Outcome::Failed {
            reason: Failure::Timer,
            downloaded: false,
        });
        return;
    }
    PENDING.with_borrow_mut(|pending| pending.push((id, request)));
}

/// 定时器到点：关掉它，取出对应的请求开始拖放
unsafe extern "system" fn on_timer(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    // SAFETY: 关掉这个只用一次的线程定时器
    let _ = unsafe { KillTimer(None, id) };
    let request = PENDING.with_borrow_mut(|pending| {
        let index = pending.iter().position(|(timer, _)| *timer == id)?;
        Some(pending.swap_remove(index).1)
    });
    if let Some(request) = request {
        run(request);
    }
}

/// 进行一次拖放，结束后调用 done
fn run(request: Request) {
    let Request {
        paths,
        labels,
        progress,
        fetch,
        done,
    } = request;
    if ACTIVE.get() {
        done(Outcome::Failed {
            reason: Failure::Busy,
            downloaded: false,
        });
        return;
    }
    ACTIVE.set(true);
    // panic 不能穿过系统的定时器回调（会直接结束进程）：在这里接住，当作拖放出错
    let outcome = catch_unwind(AssertUnwindSafe(|| drag(paths, labels, progress, fetch)))
        .unwrap_or(Outcome::Failed {
            reason: Failure::Crashed,
            downloaded: false,
        });
    ACTIVE.set(false);
    done(outcome);
}

/// 初始化 OLE 后拖放
fn drag(paths: Vec<PathBuf>, labels: Labels, progress: Arc<Progress>, fetch: Fetch) -> Outcome {
    // 主线程上 tao 已经为窗口的系统拖放初始化过 OLE，再调用返回 S_FALSE（同样要配对调用 OleUninitialize）；
    // 线程是多线程套间时返回 RPC_E_CHANGED_MODE，这时不能拖放
    // SAFETY: 参数必须为空
    if let Err(e) = unsafe { OleInitialize(None) } {
        return Outcome::Failed {
            reason: Failure::Ole(e.to_string()),
            downloaded: false,
        };
    }
    let outcome = drag_files(paths, labels, progress, fetch);
    // SAFETY: 和上面成功的 OleInitialize 配对
    unsafe { OleUninitialize() };
    outcome
}

/// 系统拖放：DoDragDrop 一直到松手（包括松手后的下载）才返回
fn drag_files(
    paths: Vec<PathBuf>,
    labels: Labels,
    progress: Arc<Progress>,
    fetch: Fetch,
) -> Outcome {
    // SAFETY: 格式名是以 0 结尾的常量
    let preferred = unsafe { RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) };
    let shared = Rc::new(Shared {
        paths: RefCell::new(paths),
        // 注册失败（返回 0）时不提供这个格式，放下的程序按它默认的效果处理
        preferred: u16::try_from(preferred).unwrap_or(0),
        decision: Cell::new(None),
    });
    let data: IDataObject = DataObject {
        shared: Rc::clone(&shared),
    }
    .into();
    let source: IDropSource = DropSource {
        shared: Rc::clone(&shared),
        origin: origin_window(),
        effect: Cell::new(DROPEFFECT_NONE.0),
        busy: Cell::new(false),
        fetch: RefCell::new(Some(fetch)),
        labels,
        progress,
    }
    .into();
    let mut effect = DROPEFFECT_NONE;
    // SAFETY: 两个对象都在本线程上创建；DoDragDrop 返回前一直在本线程上处理消息、回调它们
    let hr = unsafe {
        DoDragDrop(
            &data,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        )
    };
    outcome_of(hr, shared.decision.get())
}

/// DoDragDrop 的返回值加上松手时的决定 → 结果
fn outcome_of(hr: HRESULT, decided: Option<Decision>) -> Outcome {
    let downloaded = decided == Some(Decision::Dropped);
    if hr == DRAGDROP_S_DROP && downloaded {
        return Outcome::Dropped;
    }
    if hr == DRAGDROP_S_CANCEL || hr == DRAGDROP_S_DROP {
        return match decided {
            Some(Decision::Returned { x, y }) => Outcome::Returned { x, y },
            Some(Decision::FetchFailed) => Outcome::FetchFailed,
            // 下载好了，系统却没有放下（放下的程序最后拒绝了）：临时文件留给清理
            Some(Decision::Dropped) => Outcome::Failed {
                reason: Failure::Rejected,
                downloaded: true,
            },
            Some(Decision::Cancelled) | None => Outcome::Cancelled,
        };
    }
    Outcome::Failed {
        reason: Failure::Unexpected(format!("{hr}")),
        downloaded,
    }
}

/// 开始拖动的窗口：本线程的活动窗口（用户正在里面拖动）
fn origin_window() -> HWND {
    // SAFETY: 只读取窗口句柄
    unsafe {
        let active = GetActiveWindow();
        if active.is_invalid() {
            GetForegroundWindow()
        } else {
            active
        }
    }
}

/// 松手时的决定
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Dropped,
    Cancelled,
    Returned { x: i32, y: i32 },
    FetchFailed,
}

/// 数据对象和拖放源共用的状态（都在主线程上）
struct Shared {
    /// CF_HDROP 里的路径：开始时是全部预定的路径，下载完成后换成实际存在的
    paths: RefCell<Vec<PathBuf>>,
    /// “Preferred DropEffect” 格式的编号；注册失败时为 0，不提供这个格式
    preferred: u16,
    /// 松手时的决定（拖放结束后据此给出结果）
    decision: Cell<Option<Decision>>,
}

impl Shared {
    /// 提供的格式：CF_HDROP，以及（注册成功时）首选效果
    fn formats(&self) -> Vec<FORMATETC> {
        let mut list = vec![format_etc(CF_HDROP.0)];
        if self.preferred != 0 {
            list.push(format_etc(self.preferred));
        }
        list
    }

    /// 请求的格式是不是这里提供的
    fn check(&self, format: &FORMATETC) -> Result<(), HRESULT> {
        let known = format.cfFormat == CF_HDROP.0
            || (self.preferred != 0 && format.cfFormat == self.preferred);
        if !known {
            return Err(DV_E_FORMATETC);
        }
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT);
        }
        if (format.tymed & TYMED_HGLOBAL.0 as u32) == 0 {
            return Err(DV_E_TYMED);
        }
        Ok(())
    }

    /// 某个格式的内容：CF_HDROP 是路径列表；首选效果是“移动”——资源管理器在同一个盘上只是改名，
    /// 临时文件不会留下
    fn bytes(&self, format: u16) -> Result<Vec<u8>, HRESULT> {
        if format != CF_HDROP.0 {
            return Ok(effect_bytes(DROPEFFECT_MOVE.0));
        }
        let paths = self.paths.try_borrow().map_err(|_| E_UNEXPECTED)?;
        let wide: Vec<Vec<u16>> = paths
            .iter()
            .map(|p| p.as_os_str().encode_wide().collect())
            .collect();
        Ok(hdrop_bytes(&wide))
    }
}

/// 只描述内容、放在 HGLOBAL 里的格式
fn format_etc(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// 把字节放进一块新分配的可移动内存，所有权交给调用方（它用 ReleaseStgMedium 释放）
fn global_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: 新分配的内存至少有 bytes.len() 个字节，锁定后写入再解锁；锁定失败时释放
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let target = GlobalLock(global).cast::<u8>();
        if target.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        // 解锁到 0 时它返回“失败”而错误码是 0，结果不用看
        let _ = GlobalUnlock(global);
        Ok(global)
    }
}

/// 拖出去的数据：CF_HDROP（文件路径）和首选效果（移动）
#[implement(IDataObject, Agile = false)]
struct DataObject {
    shared: Rc<Shared>,
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: 调用方传入的格式描述，为空时报错
        let format = unsafe { pformatetcin.as_ref() }.ok_or(E_POINTER)?;
        self.shared.check(format)?;
        let bytes = self.shared.bytes(format.cfFormat)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: global_from(&bytes)?,
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: 调用方传入的格式描述，可能为空
        match unsafe { pformatetc.as_ref() } {
            Some(format) => match self.shared.check(format) {
                Ok(()) => S_OK,
                Err(hr) => hr,
            },
            None => E_POINTER,
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        // 没有另外的“规范格式”：按约定清空输出的目标设备，返回 DATA_S_SAMEFORMATETC
        // SAFETY: 调用方给的输出位置，可能为空
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> windows::core::Result<()> {
        // 放下的程序会写回一些格式（实际执行的效果、拖动图像的信息等）：收下，不用。
        // frelease 为真时介质归这里所有，要释放
        if frelease.as_bool() && !pmedium.is_null() {
            // SAFETY: 调用方把这份介质的所有权交给了这里，只释放这一次
            unsafe { ReleaseStgMedium(pmedium.cast_mut()) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: 系统按这份格式列表生成枚举器（它自己复制一份）
        unsafe { SHCreateStdEnumFmtEtc(&self.shared.formats()) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// 拖放源：决定什么时候放下；松手放到别的程序上时先下载
#[implement(IDropSource, Agile = false)]
struct DropSource {
    shared: Rc<Shared>,
    /// 开始拖动的窗口（拖回它上面松手时报告位置）
    origin: HWND,
    /// 最近一次反馈的效果：放在不接收文件的地方时是 NONE
    effect: Cell<u32>,
    /// 正在下载：这期间处理消息时如果被再次调用，一律“继续”，等下载完由外层决定
    busy: Cell<bool>,
    fetch: RefCell<Option<Fetch>>,
    labels: Labels,
    progress: Arc<Progress>,
}

impl DropSource {
    /// 记下决定，返回给 DoDragDrop 的值
    fn decide(&self, decision: Decision) -> HRESULT {
        self.shared.decision.set(Some(decision));
        if decision == Decision::Dropped {
            DRAGDROP_S_DROP
        } else {
            DRAGDROP_S_CANCEL
        }
    }

    /// 松开鼠标时：看放在哪里，放到别的程序上就先下载
    fn released(&self) -> HRESULT {
        // 拖回本程序的窗口不下载（否则窗口会把这些临时路径当成从外面拖进来的文件）
        match window_under_cursor(self.origin) {
            Under::Origin(pt) => return self.decide(Decision::Returned { x: pt.x, y: pt.y }),
            Under::OtherOwn => return self.decide(Decision::Cancelled),
            Under::Foreign => {}
        }
        if self.effect.get() == DROPEFFECT_NONE.0 {
            return self.decide(Decision::Cancelled);
        }
        let fetch = self.fetch.try_borrow_mut().ok().and_then(|mut f| f.take());
        let Some(fetch) = fetch else {
            return self.decide(Decision::Cancelled);
        };
        self.busy.set(true);
        let fetched = fetch_pumping(fetch, &self.progress, &self.labels);
        self.busy.set(false);
        match fetched {
            Some(paths) => {
                if let Ok(mut list) = self.shared.paths.try_borrow_mut() {
                    *list = paths;
                }
                self.decide(Decision::Dropped)
            }
            None => self.decide(Decision::FetchFailed),
        }
    }
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if self.busy.get() {
            return S_OK;
        }
        if fescapepressed.as_bool() {
            return self.decide(Decision::Cancelled);
        }
        if grfkeystate.contains(MK_LBUTTON) {
            return S_OK;
        }
        self.released()
    }

    fn GiveFeedback(&self, dweffect: DROPEFFECT) -> HRESULT {
        if !self.busy.get() {
            self.effect.set(dweffect.0);
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// 鼠标下面是哪里的窗口
enum Under {
    /// 开始拖动的那个窗口：松手处相对它客户区的位置（物理像素）
    Origin(POINT),
    /// 本程序的其他窗口
    OtherOwn,
    /// 别的程序（或者看不出来）
    Foreign,
}

/// 鼠标现在在哪里的窗口上（松手时调用）
fn window_under_cursor(origin: HWND) -> Under {
    // SAFETY: 只读取光标位置和窗口信息
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return Under::Foreign;
        }
        let hwnd = WindowFromPoint(pt);
        if hwnd.is_invalid() {
            return Under::Foreign;
        }
        // 网页内容是 WebView2 进程的子窗口：往上找到顶层窗口再看属于哪个进程
        let root = GetAncestor(hwnd, GA_ROOT);
        let top = if root.is_invalid() { hwnd } else { root };
        let mut pid = 0u32;
        GetWindowThreadProcessId(top, Some(&mut pid));
        if pid != GetCurrentProcessId() {
            return Under::Foreign;
        }
        if top == origin && ScreenToClient(origin, &mut pt).as_bool() {
            Under::Origin(pt)
        } else {
            Under::OtherOwn
        }
    }
}

/// 在工作线程里下载，主线程一边等一边处理消息、更新进度框。
/// 返回下载好、可以交给对方的路径；失败、取消时返回 None
fn fetch_pumping(fetch: Fetch, progress: &Arc<Progress>, labels: &Labels) -> Option<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    let worker = Arc::clone(progress);
    let spawned = std::thread::Builder::new()
        .name("drag-out".into())
        .spawn(move || {
            let _ = tx.send(fetch(worker.as_ref()));
        });
    if spawned.is_err() {
        return None;
    }
    let dialog = ProgressBox::start(labels);
    let mut cancelled_at: Option<Instant> = None;
    let fetched = loop {
        match rx.try_recv() {
            Ok(fetched) => break fetched,
            // 下载线程意外退出了
            Err(TryRecvError::Disconnected) => break None,
            Err(TryRecvError::Empty) => {}
        }
        if let Some(dialog) = &dialog {
            dialog.update(progress);
            if dialog.cancelled() {
                progress.cancel();
            }
        }
        // 取消后最多再等一会儿：网络卡住时下载线程可能迟迟看不到取消
        if progress.cancelled()
            && cancelled_at.get_or_insert_with(Instant::now).elapsed() > CANCEL_WAIT
        {
            break None;
        }
        if !pump_messages(POLL_MS, progress) {
            // 收到了退出消息（已经重新投递）：取消下载，不再等
            progress.cancel();
            break None;
        }
    };
    if let Some(dialog) = dialog {
        dialog.stop();
    }
    fetched
}

/// 等最多 timeout_ms 毫秒（有消息就提前醒来），再处理本线程已经到达的消息，窗口不会变成“未响应”。
/// 下载期间按 Esc 取消下载（拖放握着鼠标，Pierlink 窗口里的按钮点不到，只能用进度框的取消或 Esc）。
/// 返回 false 表示收到了退出消息（已经重新投递，调用方应尽快结束）
fn pump_messages(timeout_ms: u32, progress: &Progress) -> bool {
    // SAFETY: 标准的消息循环调用，只处理本线程的消息
    unsafe {
        // 不因为绘制消息醒来：有的窗口在嵌套的消息循环里处理完绘制会马上再请求绘制，按它醒来会空转
        let _ = MsgWaitForMultipleObjectsEx(
            None,
            timeout_ms,
            QS_ALLINPUT & !QS_PAINT,
            MWMO_INPUTAVAILABLE,
        );
        let mut msg = MSG::default();
        // 同样的原因，绘制消息每轮只处理一个
        let mut painted = false;
        loop {
            let filter = if painted {
                PM_REMOVE | PM_QS_INPUT | PM_QS_POSTMESSAGE | PM_QS_SENDMESSAGE
            } else {
                PM_REMOVE
            };
            if !PeekMessageW(&mut msg, None, 0, 0, filter).as_bool() {
                return true;
            }
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam.0 as i32);
                return false;
            }
            painted |= msg.message == WM_PAINT;
            if msg.message == WM_KEYDOWN && msg.wParam.0 == usize::from(VK_ESCAPE.0) {
                progress.cancel();
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 系统的进度框（IProgressDialog，在它自己的线程上显示）
struct ProgressBox {
    dialog: IProgressDialog,
    /// 第二行现在显示的文字（变了才更新，免得闪）
    shown: RefCell<String>,
}

impl ProgressBox {
    /// 打开进度框；打不开时返回 None（照样下载，只是没有进度框）
    fn start(labels: &Labels) -> Option<Self> {
        // SAFETY: 在已初始化 COM 的主线程上创建、使用；字符串参数在调用期间有效
        unsafe {
            let dialog: IProgressDialog =
                CoCreateInstance(&CLSID_ProgressDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let _ = dialog.SetTitle(&HSTRING::from(labels.title.as_str()));
            let _ = dialog.SetLine(1, &HSTRING::from(labels.line.as_str()), false, None);
            let _ = dialog.SetCancelMsg(&HSTRING::from(labels.cancelling.as_str()), None);
            // 不指定父窗口：主窗口的线程这时还被拖放占着鼠标（捕获），进度框挂在它下面的话
            // 两边的输入会连在一起，“取消”按钮点不到
            dialog
                .StartProgressDialog(None, None, PROGDLG_AUTOTIME | PROGDLG_NOMINIMIZE, None)
                .ok()?;
            let _ = dialog.Timer(PDTIMER_RESET, None);
            Some(Self {
                dialog,
                shown: RefCell::new(String::new()),
            })
        }
    }

    /// 按当前进度更新进度条和第二行（正在下载的文件）
    fn update(&self, progress: &Progress) {
        let (done, total, current) = progress.snapshot();
        // SAFETY: 主线程上使用自己创建的进度框
        unsafe {
            // 还在统计时总数是 0：进度条先停在开头
            let _ = self.dialog.SetProgress64(done, total.max(1));
            if let Ok(mut shown) = self.shown.try_borrow_mut()
                && *shown != current
            {
                let _ = self
                    .dialog
                    .SetLine(2, &HSTRING::from(current.as_str()), true, None);
                *shown = current;
            }
        }
    }

    /// 用户点了“取消”
    fn cancelled(&self) -> bool {
        // SAFETY: 同上
        unsafe { self.dialog.HasUserCancelled() }.as_bool()
    }

    /// 关掉进度框
    fn stop(self) {
        // SAFETY: 同上
        let _ = unsafe { self.dialog.StopProgressDialog() };
    }
}
