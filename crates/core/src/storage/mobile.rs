//! 手机设备存储；所有凭据比较在入口中进行，数据库不保存明文令牌。
use super::{Db, require_changed, sql_error};
use crate::{
    error::AppError,
    mobile::{Device, INACTIVE_SECONDS, Permissions},
};
use rusqlite::params;

pub(crate) struct StoredDevice {
    pub device: Device,
    pub token_hash: [u8; 32],
}

impl Db {
    /// 无参数；返回全部设备及内部认证摘要，摘要不得传给网页或日志。
    pub(crate) fn mobile_devices(&self) -> Result<Vec<StoredDevice>, AppError> {
        self.conn.prepare("SELECT id,name,can_view,can_input,can_confirm,created_at,last_active_at,revoked_at,token_hash,user_agent,last_ip FROM mobile_devices ORDER BY created_at,id")
            .map_err(sql_error)?.query_map([], |row| {
                Ok(StoredDevice {
                    device: Device {
                        id: row.get(0)?, name: row.get(1)?,
                        permissions: Permissions { view: row.get(2)?, input: row.get(3)?, confirm: row.get(4)? },
                        created_at: row.get(5)?, last_active_at: row.get(6)?, revoked_at: row.get(7)?,
                        user_agent: row.get(9)?, last_ip: row.get(10)?,
                    },
                    token_hash: row.get(8)?,
                })
            }).map_err(sql_error)?.collect::<Result<_, _>>().map_err(sql_error)
    }

    /// 写入 device 和其令牌的 hash，返回写入结果。
    pub(crate) fn insert_mobile_device(
        &self,
        device: &Device,
        hash: &[u8; 32],
    ) -> Result<(), AppError> {
        self.conn.execute("INSERT INTO mobile_devices(id,name,token_hash,can_view,can_input,can_confirm,created_at,last_active_at,user_agent,last_ip) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![device.id,device.name,hash,device.permissions.view,device.permissions.input,device.permissions.confirm,device.created_at,device.last_active_at,device.user_agent,device.last_ip]).map_err(sql_error)?;
        Ok(())
    }

    /// 同时更新 id 的 Unix 秒活动时间 now 和来源 last_ip，时间不倒退；返回更新结果。
    pub(crate) fn touch_mobile_device(
        &self,
        id: &str,
        now: i64,
        last_ip: Option<&str>,
    ) -> Result<(), AppError> {
        require_changed(self.conn.execute("UPDATE mobile_devices SET last_active_at=MAX(last_active_at,?2),last_ip=?3 WHERE id=?1 AND revoked_at IS NULL", params![id,now,last_ip]).map_err(sql_error)?)
    }

    /// 将未撤销的 id 设备改为已校验的 name，返回更新结果；不存在或已撤销返回 storage.not_found。
    pub(crate) fn rename_mobile_device(&self, id: &str, name: &str) -> Result<(), AppError> {
        require_changed(
            self.conn
                .execute(
                    "UPDATE mobile_devices SET name=?2 WHERE id=?1 AND revoked_at IS NULL",
                    params![id, name],
                )
                .map_err(sql_error)?,
        )
    }

    /// 更新未撤销 id 的 permissions，返回更新结果。
    pub(crate) fn set_mobile_permissions(
        &self,
        id: &str,
        permissions: Permissions,
    ) -> Result<(), AppError> {
        require_changed(self.conn.execute("UPDATE mobile_devices SET can_view=?2,can_input=?3,can_confirm=?4 WHERE id=?1 AND revoked_at IS NULL",
            params![id,permissions.view,permissions.input,permissions.confirm]).map_err(sql_error)?)
    }

    /// 撤销 id（None 为全部），记录 Unix 秒 now，返回更新结果。
    pub(crate) fn revoke_mobile_devices(&self, id: Option<&str>, now: i64) -> Result<(), AppError> {
        let changed = self.conn.execute("UPDATE mobile_devices SET revoked_at=COALESCE(revoked_at,?2) WHERE (?1 IS NULL OR id=?1)", params![id,now]).map_err(sql_error)?;
        if id.is_some() {
            require_changed(changed)?;
        }
        Ok(())
    }

    /// 以 Unix 秒 now 清理超出保留期限的记录，返回清理结果。
    pub(crate) fn prune_mobile_devices(&self, now: i64) -> Result<(), AppError> {
        // 撤销后保留 30 天；闲置满 30 天失效，再保留 30 天供桌面查看。
        self.conn
            .execute(
                "DELETE FROM mobile_devices WHERE revoked_at<=?1 OR last_active_at<=?2",
                params![now - INACTIVE_SECONDS, now - 2 * INACTIVE_SECONDS],
            )
            .map_err(sql_error)?;
        // 失效记录最多保留最近 128 台，配合入口的 128 台有效设备上限控制查询和存储大小。
        self.conn.execute(
            "DELETE FROM mobile_devices WHERE id IN (SELECT id FROM mobile_devices WHERE revoked_at IS NOT NULL OR last_active_at<=?1 ORDER BY COALESCE(revoked_at,last_active_at+?2) DESC,id DESC LIMIT -1 OFFSET 128)",
            params![now-INACTIVE_SECONDS,INACTIVE_SECONDS],
        ).map_err(sql_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从旧版升级手机设备信息为空且原有数据不变() {
        let root = crate::test_support::tempdir();
        let path = root.path().join("db");
        let mut old = rusqlite::Connection::open(&path).unwrap();
        use rusqlite_migration::{M, Migrations};
        Migrations::new(vec![
            M::up(include_str!("v1.sql")),
            M::up(include_str!("v2.sql")),
            M::up(include_str!("v3.sql")),
            M::up(include_str!("v4.sql")),
            M::up(include_str!("v5.sql")),
            M::up(include_str!("v6.sql")),
            M::up(include_str!("v7.sql")),
        ])
        .to_latest(&mut old)
        .unwrap();
        for (id, hash, revoked) in [
            ("active", [1u8; 32], None),
            ("revoked", [2u8; 32], Some(30)),
        ] {
            old.execute("INSERT INTO mobile_devices(id,name,token_hash,can_view,can_input,can_confirm,created_at,last_active_at,revoked_at) VALUES (?1,'旧手机',?2,0,1,1,10,20,?3)", params![id,hash,revoked]).unwrap();
        }
        drop(old);
        for _ in 0..2 {
            let db = super::super::open(&path).unwrap();
            assert_eq!(
                db.conn
                    .query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                    .unwrap(),
                8
            );
            let devices = db.mobile_devices().unwrap();
            assert_eq!(devices.len(), 2);
            for (stored, (id, hash, revoked_at)) in devices.iter().zip([
                ("active", [1u8; 32], None),
                ("revoked", [2u8; 32], Some(30)),
            ]) {
                assert_eq!(
                    stored.device,
                    Device {
                        id: id.into(),
                        name: "旧手机".into(),
                        user_agent: None,
                        last_ip: None,
                        permissions: Permissions {
                            view: false,
                            input: true,
                            confirm: true
                        },
                        created_at: 10,
                        last_active_at: 20,
                        revoked_at,
                    }
                );
                assert_eq!(stored.token_hash, hash);
            }
        }
    }

    #[test]
    fn 新建库包含手机信息并且备份恢复保留新列() {
        let root = crate::test_support::tempdir();
        let path = root.path().join("db");
        let backups = root.path().join("backups");
        let db = super::super::open(&path).unwrap();
        assert_eq!(
            db.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            8
        );
        db.conn.execute("INSERT INTO mobile_devices(id,name,token_hash,created_at,last_active_at) VALUES ('old','旧手机',?1,1,1)", [[1u8; 32]]).unwrap();
        let old = db.mobile_devices().unwrap().remove(0).device;
        assert_eq!(old.user_agent, None);
        assert_eq!(old.last_ip, None);
        let device = Device {
            id: "new".into(),
            name: "新手机".into(),
            user_agent: Some("浏览器/1".into()),
            last_ip: Some("fd7a:115c:a1e0::2".into()),
            permissions: Permissions::default(),
            created_at: 2,
            last_active_at: 3,
            revoked_at: None,
        };
        db.insert_mobile_device(&device, &[2u8; 32]).unwrap();
        db.backup_daily(&backups).unwrap();
        drop(db);
        std::fs::write(&path, b"broken database").unwrap();
        let (db, restored) = super::super::open_or_restore(&path, &backups).unwrap();
        assert!(restored.is_some());
        let devices = db.mobile_devices().unwrap();
        assert_eq!(devices[0].device, old);
        assert_eq!(devices[1].device, device);
        assert_eq!(devices[1].token_hash, [2u8; 32]);
    }

    #[test]
    fn 旧数据库升级增加手机设备且重开保留权限与撤销() {
        let root = crate::test_support::tempdir();
        let path = root.path().join("db");
        let mut old = rusqlite::Connection::open(&path).unwrap();
        use rusqlite_migration::{M, Migrations};
        Migrations::new(vec![
            M::up(include_str!("v1.sql")),
            M::up(include_str!("v2.sql")),
            M::up(include_str!("v3.sql")),
            M::up(include_str!("v4.sql")),
            M::up(include_str!("v5.sql")),
            M::up(include_str!("v6.sql")),
        ])
        .to_latest(&mut old)
        .unwrap();
        old.execute(
            "INSERT INTO settings(key,value) VALUES ('existing','true')",
            [],
        )
        .unwrap();
        drop(old);
        let db = super::super::open(&path).unwrap();
        assert_eq!(db.get_setting::<bool>("existing").unwrap(), Some(true));
        assert!(db.mobile_devices().unwrap().is_empty());
        let mut device = Device {
            id: "phone".into(),
            name: "手机".into(),
            user_agent: Some("浏览器".into()),
            last_ip: Some("100.64.0.2".into()),
            permissions: Permissions::default(),
            created_at: 1,
            last_active_at: 1,
            revoked_at: None,
        };
        db.insert_mobile_device(&device, &[7; 32]).unwrap();
        device.permissions.input = true;
        db.set_mobile_permissions(&device.id, device.permissions)
            .unwrap();
        db.touch_mobile_device(&device.id, 20, Some("100.64.0.3"))
            .unwrap();
        db.touch_mobile_device(&device.id, 10, Some("100.64.0.3"))
            .unwrap();
        device.last_active_at = 20;
        device.last_ip = Some("100.64.0.3".into());
        db.revoke_mobile_devices(Some(&device.id), 30).unwrap();
        device.revoked_at = Some(30);
        assert!(db.touch_mobile_device(&device.id, 40, None).is_err());
        assert!(
            db.set_mobile_permissions(&device.id, Permissions::default())
                .is_err()
        );
        drop(db);
        let db = super::super::open(&path).unwrap();
        assert_eq!(db.mobile_devices().unwrap()[0].device, device);
        assert_eq!(db.mobile_devices().unwrap()[0].token_hash, [7; 32]);
        assert!(
            db.conn
                .execute("UPDATE mobile_devices SET token_hash=x'01'", [])
                .is_err()
        );
        assert!(
            db.conn
                .execute("UPDATE mobile_devices SET can_input=2", [])
                .is_err()
        );
        db.prune_mobile_devices(30 + INACTIVE_SECONDS).unwrap();
        assert!(db.mobile_devices().unwrap().is_empty());
    }
}
