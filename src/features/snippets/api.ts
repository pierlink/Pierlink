import { inTauri } from "../shell/TitleBar";

/** 一条命令片段；groupName 为空时所有服务器通用 */
export interface SnippetRecord {
  id: string;
  title: string;
  body: string;
  groupName: string | null;
  sortOrder: number;
}

/** 命令片段的存取接口：Tauri 实现和浏览器预览实现都遵守它 */
export interface SnippetsApi {
  list(): Promise<SnippetRecord[]>;
  /** 新建（id 为空）或修改；返回保存后的记录 */
  save(s: Omit<SnippetRecord, "id"> & { id: string | null }): Promise<SnippetRecord>;
  remove(id: string): Promise<void>;
}

/** 浏览器预览：存在内存里，带几条示例 */
function previewApi(): SnippetsApi {
  let items: SnippetRecord[] = [
    { id: "s1", title: "切到实验分支并拉取", body: "git switch {{branch}} && git pull --ff-only", groupName: "开发", sortOrder: 0 },
    { id: "s2", title: "看 GPU 占用", body: "watch -n 2 nvidia-smi", groupName: "开发", sortOrder: 1 },
    { id: "s3", title: "磁盘最大的 10 个目录", body: "du -h --max-depth=1 . | sort -rh | head", groupName: null, sortOrder: 0 },
  ];
  let seq = 10;
  return {
    list: async () => items.map((x) => ({ ...x })),
    save: async (s) => {
      const rec: SnippetRecord = { ...s, id: s.id ?? `s${++seq}` };
      items = items.some((x) => x.id === rec.id) ? items.map((x) => (x.id === rec.id ? rec : x)) : [...items, rec];
      return rec;
    },
    remove: async (id) => { items = items.filter((x) => x.id !== id); },
  };
}

let api: Promise<SnippetsApi> | null = null;
/** 取得命令片段接口（Tauri 里用应用后端，浏览器预览用内存实现） */
export function getSnippetsApi(): Promise<SnippetsApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.tauriSnippetsApi) : Promise.resolve(previewApi());
  return api;
}
