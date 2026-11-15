; NSIS 钩子。Tauri 的安装脚本在合适的位置插入这些宏。
;
; 开机自启由应用自己在启动时按设置写入 HKCU\...\Run，
; 这样重新安装到别的目录后，路径会被自动纠正。
; 卸载时必须清掉，否则会留下一条指向已删除 exe 的启动项。

!macro NSIS_HOOK_PREUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "IDM Shadow"
  DeleteRegKey HKCU "Software\Classes\idmshadow"
!macroend
