import { useCallback, useEffect, useState } from "react";
import { getServersApi, notifyServersChanged, onServersChanged, type ServerInputView, type ServerRecord } from "./api";

/**
 * 服务器列表及增删改操作。每次修改后重新读取列表，保证与数据库一致。
 * @returns 列表、读取错误、以及保存/删除/收藏函数（失败时抛出后端错误，由调用方展示）
 */
export function useServers() {
  const [servers, setServers] = useState<ServerRecord[]>([]);
  const [loadError, setLoadError] = useState<unknown>(null);
  /** 列表已经成功读取过至少一次（之前的空列表不代表“没有服务器”） */
  const [loaded, setLoaded] = useState(false);

  const reload = useCallback(async () => {
    try {
      const api = await getServersApi();
      setServers(await api.list());
      setLoaded(true);
      setLoadError(null);
    } catch (e) {
      setLoadError(e);
    }
  }, []);

  useEffect(() => { void reload(); }, [reload]);
  // 别处改了服务器记录（如终端里改用系统 OpenSSH）：重新读取
  useEffect(() => onServersChanged(() => void reload()), [reload]);

  const save = useCallback(async (input: ServerInputView) => {
    const api = await getServersApi();
    const saved = await api.save(input);
    await reload();
    // 别处（如资源概览）据此发现连接方式等变化
    notifyServersChanged();
    return saved;
  }, [reload]);

  const remove = useCallback(async (id: string) => {
    await (await getServersApi()).remove(id);
    await reload();
  }, [reload]);

  const setFavorite = useCallback(async (id: string, favorite: boolean) => {
    await (await getServersApi()).setFavorite(id, favorite);
    await reload();
  }, [reload]);

  const setCommandPrediction = useCallback(async (id: string, enabled: boolean) => {
    await (await getServersApi()).setCommandPrediction(id, enabled);
    await reload();
  }, [reload]);

  return { servers, loaded, loadError, reload, save, remove, setFavorite, setCommandPrediction };
}
