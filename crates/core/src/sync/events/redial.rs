//! 地址重拨不产生人为断连；事件泵保留真实断开、恢复及其他事件。
use super::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn 地址重拨保留真实设备事件_跨批失败重试不丢失事件() {
    let fail = Arc::new(AtomicBool::new(true));
    let flag = fail.clone();
    let mut client = Client::new("http://127.0.0.1:1", "test-key").unwrap();
    client.test_handler = Some(Arc::new(move |_, path, _, _| match path {
        "/rest/system/status" => Ok(json!({"startTime": "启动"})),
        "/rest/events" => Ok(json!([
            {"id":11,"type":"DeviceDisconnected","time":"2026-10-24T00:00:01Z","data":{"id":"甲"}},
            {"id":12,"type":"DeviceConnected","time":"2026-10-24T00:00:02Z","data":{"id":"甲"}},
            {"id":13,"type":"LocalIndexUpdated","time":"2026-10-24T00:00:03Z","data":{"folder":"f","filenames":["a"]}},
            {"id":14,"type":"DeviceDisconnected","time":"2026-10-24T00:00:04Z","data":{"id":"甲"}},
            {"id":15,"type":"DeviceConnected","time":"2026-10-24T00:00:05Z","data":{"id":"乙"}}
        ])),
        "/rest/db/file" if flag.load(Ordering::Acquire) => {
            Err(AppError::new("syncthing.unreachable"))
        }
        "/rest/db/file" => Ok(json!({})),
        _ => panic!("未预期接口"),
    }));
    let local = LocalSync {
        address_updates: Default::default(),
        instance: None,
        client,
        id: "本机".into(),
    };
    let mut pump = EventPump::new(&local);
    pump.initialized = true;
    pump.since = 10;
    assert!(pump.poll().is_err());
    assert_eq!(pump.since, 10);
    fail.store(false, Ordering::Release);
    let events = pump.poll().unwrap();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0].kind, SyncEventKind::DeviceDisconnected);
    assert_eq!(events[1].kind, SyncEventKind::DeviceConnected);
    assert_eq!(events[2].kind, SyncEventKind::StateChanged);
    assert_eq!(events[3].kind, SyncEventKind::DeviceDisconnected);
    assert_eq!(events[4].device.as_deref(), Some("乙"));
    assert_eq!(pump.since, 15);
    assert!(pump.poll().unwrap().is_empty());
}
