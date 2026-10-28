import { useState } from "react";
import { UninstallDialog } from "../features/sync/UninstallDialog";

const MB = 1024 * 1024;

/** 开发预览：卸载服务器端同步组件（#sync-uninstall） */
export function UninstallDemo() {
  const [open, setOpen] = useState(true);
  if (!open) return null;
  return (
    <UninstallDialog
      busy={false}
      report={{
        serverName: "gpu-dev",
        remove: [
          { path: "~/.local/share/pierlink/bin/v2.1.7/x86_64/", kind: "program", bytes: 33 * MB },
          { path: "~/.local/state/pierlink/3f9c2a7e-…/home/", kind: "instance", bytes: 46 * MB },
          { path: "~/.config/systemd/user/pierlink-3f9c2a7e.service", kind: "service", bytes: null },
        ],
        versionsBytes: 12 * MB,
        conflictsBytes: 1.5 * MB,
        tasks: [
          { name: "web-api", remotePath: "~/projects/web-api", localPath: "D:\\code\\web-api" },
          { name: "ml-train", remotePath: "~/ml-train", localPath: "D:\\code\\ml-train" },
        ],
        otherComputers: ["DESKTOP-OFFICE"],
        ownSyncthing: { binary: "/usr/bin/syncthing", configDir: "~/.local/state/syncthing" },
        hasDedicatedKey: true,
      }}
      onConfirm={() => setOpen(false)}
      onClose={() => setOpen(false)}
    />
  );
}
