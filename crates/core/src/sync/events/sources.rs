//! 变化事件携带作者，上传事件必须确认同一版本已出现在目标设备。
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
const LOCAL: &str = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
const REMOTE: &str = "BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB";
fn event(id: u64, kind: &str, data: Value) -> Value {
    json!({"id":id,"type":kind,"time":"2026-10-25T00:00:00Z","data":data})
}
fn pump(batch: Arc<Mutex<Value>>, info: Arc<Mutex<Value>>) -> EventPump {
    let mut client = Client::new("http://127.0.0.1:1", "测试密钥").unwrap();
    client.test_handler = Some(Arc::new(move |_, path, query, _| match path {
        "/rest/system/status" => Ok(json!({"startTime":"开始"})),
        "/rest/events" => {
            assert!(
                query
                    .iter()
                    .any(|(k, v)| *k == "events" && *v == EVENT_TYPES)
            );
            Ok(batch.lock().unwrap().clone())
        }
        "/rest/config/devices" => Ok(json!([{"deviceID":LOCAL},{"deviceID":REMOTE}])),
        "/rest/db/file" => Ok(info.lock().unwrap().clone()),
        _ => panic!("未预期接口 {path}"),
    }));
    let local = LocalSync {
        client,
        instance: None,
        id: LOCAL.into(),
        address_updates: Default::default(),
    };
    let mut pump = EventPump::new(&local);
    pump.initialized = true;
    pump
}
#[test]
fn 下载删除冲突作者与传输设备分离且不重复完成事件() {
    let batch = Arc::new(Mutex::new(json!([
        event(
            1,
            "LocalChangeDetected",
            json!({"folder":"f","path":"local-delete","action":"deleted","type":"file"})
        ),
        event(
            2,
            "RemoteChangeDetected",
            json!({"folder":"f","path":"download","action":"modified","type":"file","modifiedBy":"BBBBBBB","device":LOCAL})
        ),
        event(
            3,
            "ItemFinished",
            json!({"folder":"f","item":"download","action":"update","type":"file"})
        ),
        event(
            4,
            "RemoteChangeDetected",
            json!({"folder":"f","path":"remote-delete","action":"deleted","type":"file","modifiedBy":"BBBBBBB"})
        ),
        event(
            5,
            "RemoteChangeDetected",
            json!({"folder":"f","path":"a.sync-conflict-copy","action":"added","type":"file","modifiedBy":REMOTE})
        ),
        event(
            6,
            "RemoteChangeDetected",
            json!({"folder":"f","path":"unknown","action":"added","type":"file","modifiedBy":"CCCCCCC"})
        ),
        event(
            7,
            "LocalChangeDetected",
            json!({"folder":"f","path":"dir","action":"deleted","type":"dir"})
        ),
        event(
            8,
            "RemoteChangeDetected",
            json!({"folder":"f","path":"missing","action":"modified","type":"file"})
        )
    ])));
    let mut pump = pump(batch, Arc::new(Mutex::new(json!({}))));
    let result = pump.poll().unwrap();
    assert_eq!(result.len(), 6);
    assert_eq!(result[0].by.as_deref(), Some("local"));
    assert_eq!(result[0].kind, SyncEventKind::Deleted);
    assert_eq!(result[1].by.as_deref(), Some(REMOTE));
    assert_eq!(result[1].device.as_deref(), Some(LOCAL));
    assert_eq!(result[1].kind, SyncEventKind::Downloaded);
    assert_eq!(result[2].by.as_deref(), Some(REMOTE));
    assert_eq!(result[2].kind, SyncEventKind::Deleted);
    assert_eq!(result[3].kind, SyncEventKind::Conflict);
    assert_eq!(result[3].by.as_deref(), Some(REMOTE));
    assert!(result[4].by.is_none() && result[5].by.is_none());
    assert!(pump.poll().unwrap().is_empty());
}
#[test]
fn 上传跨批保留作者且版本改变不能套用旧作者() {
    for (author, expected) in [
        ("AAAAAAA", Some("local")),
        ("BBBBBBB", Some(REMOTE)),
        ("CCCCCCC", None),
    ] {
        let batch = Arc::new(Mutex::new(json!([event(
            1,
            "LocalIndexUpdated",
            json!({"folder":"f","filenames":["file"]})
        )])));
        let info = Arc::new(Mutex::new(
            json!({"local":{"modifiedBy":author,"version":["a:1"]},"global":{"version":["a:1"]},"availability":[{"id":REMOTE,"fromTemporary":false}]}),
        ));
        let mut pump = pump(batch.clone(), info.clone());
        pump.poll().unwrap();
        *batch.lock().unwrap() = json!([event(
            2,
            "RemoteIndexUpdated",
            json!({"folder":"f","device":REMOTE})
        )]);
        let result = pump.poll().unwrap();
        let uploaded = result
            .iter()
            .find(|v| v.kind == SyncEventKind::Uploaded)
            .unwrap();
        assert_eq!(uploaded.by.as_deref(), expected);
        assert_eq!(uploaded.device.as_deref(), Some(REMOTE));
        *batch.lock().unwrap() = json!([event(
            3,
            "LocalIndexUpdated",
            json!({"folder":"f","filenames":["file"]})
        )]);
        pump.poll().unwrap();
        *info.lock().unwrap() = json!({"local":{"modifiedBy":"BBBBBBB","version":["a:2"]},"global":{"version":["a:2"]},"availability":[{"id":REMOTE,"fromTemporary":false}]});
        *batch.lock().unwrap() = json!([event(
            4,
            "RemoteIndexUpdated",
            json!({"folder":"f","device":REMOTE})
        )]);
        assert!(
            pump.poll()
                .unwrap()
                .iter()
                .all(|v| v.kind != SyncEventKind::Uploaded)
        );
    }
}
#[test]
fn 短设备编号碰撞与查不到设备不猜作者且兼容旧事件() {
    let client = Client::new("http://127.0.0.1:1", "测试密钥").unwrap();
    let collision = REMOTE.replacen("-BBBBBBB", "-CCCCCCC", 1);
    let mut cache = Some(vec![REMOTE.into(), collision]);
    assert_eq!(resolve_source(&client, LOCAL, "BBBBBBB", &mut cache), None);
    let mut cache = Some(vec![]);
    for source in ["", "secret", "local", "BBBBBB!", "CCCCCCC"] {
        assert_eq!(resolve_source(&client, LOCAL, source, &mut cache), None);
    }
    assert_eq!(
        resolve_source(&client, LOCAL, LOCAL, &mut cache).as_deref(),
        Some("local")
    );
    let old: SyncEvent=serde_json::from_value(json!({"folder_id":"f","time_ms":1,"kind":"Deleted","device":REMOTE,"path":"a","detail":null})).unwrap();
    assert!(old.by.is_none());
}
