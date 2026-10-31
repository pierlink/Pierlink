/** 工作区界面用的视图数据。接入后端后由生成的接口类型转换而来。 */

export type ConnStatus = "online" | "connecting" | "offline";

export interface ServerView {
  id: string;
  name: string;
  /** 显示在名称下方的地址，如 dev@203.0.113.18 */
  address: string;
  group: string | null;
  favorite: boolean;
  status: ConnStatus;
  openTabs: number;
  /** 导入自 ssh config（连接参数以配置文件为准） */
  linked?: boolean;
  /** 这台服务器关掉了命令预测（全局设置开着也不预测、不装提示符钩子） */
  noPrediction?: boolean;
  syncState: "none" | "ok" | "busy" | "warn";
}

export interface TabView {
  id: string;
  kind: "remote" | "local";
  title: string;
  /** 当前目录或说明 */
  subtitle: string;
  status: ConnStatus;
  badge?: string;
  /** 这个标签里有 AI 打开的终端 */
  ai?: boolean;
}

export interface ResourceView {
  label: string;
  value: string;
  /** 0–100 */
  percent: number;
  detail: string;
  tone: "accent" | "warn";
}

export interface ProjectView {
  id: string;
  name: string;
  remotePath: string;
  localPath: string | null;
  state: string;
  tone: "ok" | "accent" | "warn";
  percent: number;
  detail: string;
  conflicts: number;
}
