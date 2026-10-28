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
        self.conn.prepare("SELECT id,name,can_view,can_input,can_confirm,created_at,last_active_at,revoked_at,token_hash FROM mobile_devices ORDER BY created_at,id")
            .map_err(sql_error)?.query_map([], |row| {
                Ok(StoredDevice {
                    device: Device {
                        id: row.get(0)?, name: row.get(1)?,
                        permissions: Permissions { view: row.get(2)?, input: row.get(3)?, confirm: row.get(4)? },
                        created_at: row.get(5)?, last_active_at: row.get(6)?, revoked_at: row.get(7)?,
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
        self.conn.execute("INSERT INTO mobile_devices(id,name,token_hash,can_view,can_input,can_confirm,created_at,last_active_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![device.id,device.name,hash,device.permissions.view,device.permissions.input,device.permissions.confirm,device.created_at,device.last_active_at]).map_err(sql_error)?;
        Ok(())
    }

    /// 将 id 的活动时间更新为 Unix 秒 now，不倒退；返回更新结果。
    pub(crate) fn touch_mobile_device(&self, id: &str, now: i64) -> Result<(), AppError> {
        require_changed(self.conn.execute("UPDATE mobile_devices SET last_active_at=MAX(last_active_at,?2) WHERE id=?1 AND revoked_at IS NULL", params![id,now]).map_err(sql_error)?)
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
            permissions: Permissions::default(),
            created_at: 1,
            last_active_at: 1,
            revoked_at: None,
        };
        db.insert_mobile_device(&device, &[7; 32]).unwrap();
        device.permissions.input = true;
        db.set_mobile_permissions(&device.id, device.permissions)
            .unwrap();
        db.touch_mobile_device(&device.id, 20).unwrap();
        db.touch_mobile_device(&device.id, 10).unwrap();
        device.last_active_at = 20;
        db.revoke_mobile_devices(Some(&device.id), 30).unwrap();
        device.revoked_at = Some(30);
        assert!(db.touch_mobile_device(&device.id, 40).is_err());
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
