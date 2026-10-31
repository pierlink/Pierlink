//! 将预览过的服务器和跳板在一个事务中保存，防止部分转换。
use super::*;

impl Db {
    pub(crate) fn apply_conversion(
        &mut self,
        expected: &[Server],
        updates: &[Server],
        additions: &[Server],
    ) -> Result<(), AppError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let current = tx
            .prepare("SELECT * FROM servers ORDER BY sort_order, name, id")
            .map_err(sql_error)?
            .query_map([], Server::from_row)
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        if current != expected {
            return Err(AppError::new("ssh.conversion_changed"));
        }
        for server in additions {
            validate_server(server)?;
            writes::insert_servers(&tx, server)?;
        }
        for server in updates {
            validate_server(server)?;
            writes::update_servers(&tx, server)?;
        }
        tx.commit().map_err(sql_error)
    }
}
