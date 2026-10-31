; Pierlink 安装/卸载钩子（由 Tauri 的 NSIS 模板在对应位置插入）
;
; 安装或卸载前结束 Pierlink 自己启动的本机同步服务（syncthing.exe）：
; 应用被安装程序关闭时它可能还在运行，会占着要覆盖或删除的 syncthing.exe。
; 只结束程序路径在安装目录里的 syncthing.exe，用户自己装的 Syncthing 不受影响。
; 安装目录经环境变量传给 PowerShell，路径里有引号等字符也不会出错。

!macro PierlinkStopSyncthing
  System::Call 'Kernel32::SetEnvironmentVariable(t "PIERLINK_INSTDIR", t "$INSTDIR")'
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$d = [IO.Path]::GetFullPath($$env:PIERLINK_INSTDIR).TrimEnd('\') + '\'; Get-Process -Name syncthing -ErrorAction SilentlyContinue | Where-Object { $$_.Path -and $$_.Path.StartsWith($$d, [StringComparison]::OrdinalIgnoreCase) } | Stop-Process -Force -ErrorAction SilentlyContinue"`
  Pop $0
  Sleep 500
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro PierlinkStopSyncthing
!macroend

; 安装完成后记下这次的界面语言：首次安装由用户在语言选择框里选，之后安装和自动更新从这个值读取、不再弹框；
; 卸载程序也按这个值显示语言。模板没有插入 MUI_LANGDLL_SAVELANGUAGE，所以在这里写。
!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "${MANUPRODUCTKEY}" "Installer Language" $LANGUAGE
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro PierlinkStopSyncthing
!macroend

; 卸载后：清掉旧数据目录里残留的锁文件和空目录。
; 勾选了“同时删除我的数据”时，把还没搬到新数据目录的旧数据也删掉（升级后没打开过就卸载的情况）。
; 旧数据只在默认安装目录（%LOCALAPPDATA%\Pierlink）里，安装到别处时不碰，避免误删同名目录。
!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\syncthing\pierlink.lock"
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
  ${AndIf} $INSTDIR == "$LOCALAPPDATA\Pierlink"
    Delete "$INSTDIR\pierlink.db"
    Delete "$INSTDIR\pierlink.db-wal"
    Delete "$INSTDIR\pierlink.db-shm"
    Delete "$INSTDIR\pierlink.db-journal"
    RMDir /r "$INSTDIR\backups"
    RMDir /r "$INSTDIR\versions"
    RMDir /r "$INSTDIR\conflicts"
    RMDir /r "$INSTDIR\keys"
    RMDir /r "$INSTDIR\ssh-configs"
    RMDir /r "$INSTDIR\shell-hooks"
    RMDir /r "$INSTDIR\transfer-tmp"
    RMDir /r "$INSTDIR\config"
    RMDir /r "$INSTDIR\.ssh"
    RMDir /r "$INSTDIR\recovery"
    RMDir /r "$INSTDIR\syncthing"
  ${EndIf}
  RMDir "$INSTDIR\syncthing"
  RMDir "$INSTDIR"
!macroend
