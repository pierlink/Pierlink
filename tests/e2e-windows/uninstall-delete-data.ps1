# 用卸载程序的界面卸载 Pierlink，并勾选“同时删除我的数据”，然后检查数据目录和安装目录都没了。
# 静默卸载（/S）不会显示这个勾选框（等于不删数据），所以这里用 Windows UI 自动化操作真实的卸载窗口。
#
# 环境变量：E2E_OUT          截图输出目录
#           E2E_CASE         截图子目录名（默认“卸载删除数据”）
#           E2E_EXPECT_DATA  数据在哪：new（默认，新位置）/ old（升级后还没打开过新版，数据仍在安装目录）
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing

$case = if ($env:E2E_CASE) { $env:E2E_CASE } else { '卸载删除数据' }
$out = Join-Path $env:E2E_OUT "阶段5-$case"
New-Item -ItemType Directory -Force $out | Out-Null
$installDir = Join-Path $env:LOCALAPPDATA 'Pierlink'
$dataDir = Join-Path $env:LOCALAPPDATA 'com.pierlink.desktop'
$uninstaller = Join-Path $installDir 'uninstall.exe'

function Save-Screen([string]$name) {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
  $bmp.Save((Join-Path $out "$name.png"))
  $g.Dispose(); $bmp.Dispose()
}

# 在桌面上找名字里带 Pierlink 的卸载窗口（卸载程序会把自己复制到临时目录再运行，进程号会变，所以按窗口找）
function Find-Window([int]$seconds) {
  $root = [System.Windows.Automation.AutomationElement]::RootElement
  $deadline = (Get-Date).AddSeconds($seconds)
  while ((Get-Date) -lt $deadline) {
    $all = $root.FindAll([System.Windows.Automation.TreeScope]::Children, [System.Windows.Automation.Condition]::TrueCondition)
    foreach ($w in $all) {
      # 没记下安装语言时卸载程序会先弹这个窗口：用户也会看到，算失败
      if ($w.Current.Name -eq 'Installer Language') { Save-Screen '弹出了选择语言窗口'; throw '卸载程序先弹出了“Installer Language”选择语言窗口（安装时没有记下语言）' }
      if ($w.Current.Name -like '*Pierlink*' -and $w.Current.ClassName -eq '#32770') { return $w }
    }
    Start-Sleep -Milliseconds 300
  }
  return $null
}

# 在窗口里按控件类型和名字（前缀）找控件，找不到返回 $null
function Find-Control($window, $type, [string]$namePrefix, [int]$seconds = 15) {
  $cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ControlTypeProperty, $type)
  $deadline = (Get-Date).AddSeconds($seconds)
  while ((Get-Date) -lt $deadline) {
    foreach ($c in $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)) {
      if ($c.Current.Name.StartsWith($namePrefix) -and $c.Current.IsEnabled) { return $c }
    }
    Start-Sleep -Milliseconds 300
  }
  return $null
}

function Click($element) {
  $element.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
}

if (-not (Test-Path $uninstaller)) { throw "找不到卸载程序：$uninstaller" }
$expectDir = if ($env:E2E_EXPECT_DATA -eq 'old') { $installDir } else { $dataDir }
if (-not (Test-Path (Join-Path $expectDir 'pierlink.db'))) { throw "卸载前这里应当有数据库：$expectDir" }

Write-Host "启动卸载程序：$uninstaller"
Start-Process $uninstaller
$window = Find-Window 30
if (-not $window) { Save-Screen '找不到卸载窗口'; throw '30 秒内没有出现卸载窗口' }
Write-Host "卸载窗口：$($window.Current.Name)"

$check = Find-Control $window ([System.Windows.Automation.ControlType]::CheckBox) 'Also delete my data'
if (-not $check) { Save-Screen '找不到删除数据勾选框'; throw '卸载窗口里没有“同时删除我的数据”勾选框' }
$toggle = $check.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
if ($toggle.Current.ToggleState -ne [System.Windows.Automation.ToggleState]::On) { $toggle.Toggle() }
if ($toggle.Current.ToggleState -ne [System.Windows.Automation.ToggleState]::On) { throw '勾选“同时删除我的数据”失败' }
Save-Screen '01-已勾选删除数据'

$uninstall = Find-Control $window ([System.Windows.Automation.ControlType]::Button) 'Uninstall'
if (-not $uninstall) { Save-Screen '找不到卸载按钮'; throw '卸载窗口里没有“Uninstall”按钮' }
Click $uninstall

# 卸载完成后会有“完成/关闭”按钮；点掉它
$done = Find-Control $window ([System.Windows.Automation.ControlType]::Button) 'Finish' 60
if (-not $done) { $done = Find-Control $window ([System.Windows.Automation.ControlType]::Button) 'Close' 5 }
Save-Screen '02-卸载完成'
if ($done) { Click $done }

$deadline = (Get-Date).AddSeconds(30)
while ((Test-Path $dataDir) -or (Test-Path $uninstaller)) {
  if ((Get-Date) -gt $deadline) { break }
  Start-Sleep -Milliseconds 500
}
$left = @()
if (Test-Path $dataDir) { $left += "数据目录还在：$dataDir（$((Get-ChildItem $dataDir -Recurse -Force | Select-Object -First 10 | ForEach-Object FullName) -join ', ')）" }
if (Test-Path $installDir) { $left += "安装目录还在：$installDir（$((Get-ChildItem $installDir -Recurse -Force | Select-Object -First 10 | ForEach-Object FullName) -join ', ')）" }
if ($left.Count) { throw ($left -join '；') }
Write-Host '卸载并删除数据：数据目录和安装目录都已删除'
