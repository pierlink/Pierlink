import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Code2, FolderOpen, Plus, RefreshCw, Trash2 } from "lucide-react";
import {
  Badge, Banner, Button, Checkbox, Dialog, IconButton, Segmented, SelectField, SplitButton, StatusDot, Switch, TextField,
  useContextMenu, type MenuEntry,
} from "../components";
import i18n from "../i18n";
import { applyTheme, type ThemePref } from "../theme";

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="pl-card" style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      <h2 style={{ margin: 0, fontSize: "var(--fs-xs)", letterSpacing: 1, color: "var(--text-dim)" }}>{title}</h2>
      <div style={{ display: "flex", flexWrap: "wrap", gap: 10, alignItems: "center" }}>{children}</div>
    </section>
  );
}

/** 编辑器菜单示例：勾选的编辑器 + 其他打开方式（设计 3.5 的统一规则） */
const editorEntries: MenuEntry[] = [
  { id: "vscode", label: "VS Code", checked: true },
  { id: "cursor", label: "Cursor" },
  { id: "pycharm", label: "PyCharm" },
  { kind: "separator", id: "s1" },
  { id: "other", label: "其他打开方式… / Other…" },
];

/**
 * 内部组件预览页：同时检查深浅色主题和中英文，开发模式下才显示。
 */
export function ComponentGallery() {
  const [conflict, setConflict] = useState("ask");
  const { t } = useTranslation();
  const [theme, setTheme] = useState<ThemePref>("dark");
  const [lang, setLang] = useState(i18n.language);
  const [sw, setSw] = useState(true);
  const [dialog, setDialog] = useState(false);
  const ctx = useContextMenu("示例右键菜单");

  const fileMenu: MenuEntry[] = [
    { id: "edit", label: "编辑（软件内）", shortcut: "Enter" },
    { id: "download", label: "下载" },
    { kind: "separator", id: "s1" },
    { id: "vscode", label: "用 VS Code 打开", disabled: true },
    { kind: "hint", id: "h1", label: "这个文件被排除规则排除，没有同步到本地，不能用本机编辑器打开" },
    { kind: "separator", id: "s2" },
    { id: "copy", label: "复制路径", shortcut: "Ctrl Shift C" },
    { kind: "separator", id: "s3" },
    { id: "del", label: "删除", danger: true, shortcut: "Del" },
  ];

  return (
    <div className="pl-panel" style={{ flex: 1, overflow: "auto", padding: 20, display: "flex", flexDirection: "column", gap: 14 }}>
      <div style={{ display: "flex", gap: 12, alignItems: "center", flexWrap: "wrap" }}>
        <h1 style={{ margin: 0, fontSize: "var(--fs-xl)", flex: 1 }}>{t("nav.gallery")}</h1>
        <Segmented
          label={t("settings.theme")}
          value={theme}
          onChange={(v) => { setTheme(v); applyTheme(v); }}
          options={[{ value: "dark", label: t("settings.themeDark") }, { value: "light", label: t("settings.themeLight") }, { value: "system", label: t("settings.themeSystem") }]}
        />
        <Segmented
          label={t("settings.language")}
          value={lang}
          onChange={(v) => { setLang(v); void i18n.changeLanguage(v); }}
          options={[{ value: "zh-CN", label: "简体中文" }, { value: "en", label: "English" }]}
        />
      </div>

      <Section title="按钮 Buttons">
        <Button variant="primary" icon={<Plus size={14} />}>新建同步任务</Button>
        <Button>{t("common.cancel")}</Button>
        <Button variant="ghost">{t("common.more")}</Button>
        <Button variant="soft">设置同步…</Button>
        <Button variant="danger" icon={<Trash2 size={14} />}>{t("common.delete")}</Button>
        <Button variant="danger-ghost">卸载同步组件…</Button>
        <Button size="sm">{t("common.retry")}</Button>
        <Button size="lg" variant="primary">创建并开始同步</Button>
        <Button disabled>{t("common.save")}</Button>
        <IconButton label="重新扫描"><RefreshCw size={14} /></IconButton>
        <SplitButton label="VS Code" icon={<Code2 size={13} />} onClick={() => {}} moreLabel="选择其他编辑器" menuLabel="打开方式" entries={editorEntries} />
      </Section>

      <Section title="状态 Status">
        <Switch checked={sw} onChange={setSw} label="窗口毛玻璃效果" />
        <Badge>已暂停</Badge>
        <Badge tone="accent">同步中 62%</Badge>
        <Badge tone="ok">已同步</Badge>
        <Badge tone="warn">1 个冲突</Badge>
        <Badge tone="danger">连接中断</Badge>
        <StatusDot tone="ok" label="已连接" />
        <StatusDot tone="warn" label="连接中" />
        <StatusDot tone="danger" label="出错" />
        <StatusDot label="未连接" />
      </Section>

      <Section title="表单 Forms">
        <div style={{ display: "grid", gridTemplateColumns: "repeat(3, minmax(0, 1fr))", gap: 12, width: "100%" }}>
          <TextField label="主机地址" defaultValue="203.0.113.18" className="pl-input mono" />
          <SelectField label="冲突处理" value={conflict} onChange={setConflict} options={[{ value: "ask", label: "每次提示我" }, { value: "newest", label: "自动保留最新修改的版本" }]} />
          <div style={{ alignSelf: "end" }}><Checkbox label="我已核对目录，并了解删除会同步到另一端" defaultChecked /></div>
        </div>
      </Section>

      <Section title="提示 Banners">
        <div style={{ display: "flex", flexDirection: "column", gap: 8, width: "100%" }}>
          <Banner tone="info" title="首次同步：服务器 → 本机">本地目录是空的，会先下载服务器上的文件。</Banner>
          <Banner tone="warn" title="两端都有文件，将合并" actions={<Button size="sm">查看冲突文件</Button>}>同名但内容不同的文件会按冲突处理设置处理，首次合并不删除任何文件。</Banner>
          <Banner tone="danger" title="网络已断开">恢复后自动重连；普通终端会话需按 Enter 重连。</Banner>
        </div>
      </Section>

      <Section title="菜单与对话框 Menus & dialogs">
        <div
          className="pl-card"
          style={{ width: 360, height: 90, display: "grid", placeItems: "center", color: "var(--text-dim)", cursor: "context-menu" }}
          onContextMenu={(e) => ctx.open(e, fileMenu)}
        >
          <span><FolderOpen size={14} style={{ verticalAlign: -2 }} /> 在这里点右键</span>
        </div>
        <Button onClick={() => setDialog(true)}>打开对话框示例</Button>
      </Section>

      {ctx.node}
      {dialog && (
        <Dialog
          title="关闭 gpu-dev 的终端？"
          onDismiss={() => setDialog(false)}
          actions={<>
            <Button onClick={() => setDialog(false)}>{t("common.cancel")}</Button>
            <Button variant="primary" onClick={() => setDialog(false)}>关闭标签</Button>
          </>}
        >
          关闭会断开 SSH，前台和后台作业都会结束；文件同步不受影响。
        </Dialog>
      )}
    </div>
  );
}
