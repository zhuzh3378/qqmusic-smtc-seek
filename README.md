# qqmusic-smtc-seek

让 **QQ 音乐 Windows 端**的 SMTC 会话支持进度跳转 —— 也就是能在 Windows 的音量飞控、锁屏、任务栏预览里**拖动播放进度条**。

QQ 音乐 22.x 本身支持 SMTC(能显示歌名、能上/下曲),但进度条是灰的。这个项目补上这一块,而且**不改变会话身份**:外部看到的仍然是 `QQMusic.exe` 这一个会话,不是另起一个代理会话。

顺带保留了代码基线自带的两个功能:把歌曲 ID 写进 SMTC 的"流派"字段(格式 `QQ-{ID}`,便于外部精确匹配),以及把封面从 300×300 提升到 1500×1500。

---

## 为什么 QQ 音乐不能拖进度

两个卡点都在应用侧,外部工具无从绕过:

1. 它调用 `UpdateTimelineProperties` 时**不填** `MinSeekTime` / `MaxSeekTime`。按微软的规定,这两个值不填,系统就不会向该会话派发 `PlaybackPositionChangeRequested`。
2. 它也没有注册 `PlaybackPositionChangeRequested` 回调。

所以任何 SMTC 客户端读到的 `CanChangePlaybackPosition` 都是 `false`,`TryChangePlaybackPositionAsync` 返回 `false`。

## 做法

注入进 QQ 音乐进程,通过 `SystemMediaTransportControlsInterop::GetForWindow` 拿到**它自己的**那个 SMTC 实例,然后:

**第一步 —— 对外声明能力。** Hook `ISystemMediaTransportControls::UpdateTimelineProperties`(改的是接口 vtable 里的实现地址,进程内所有实例都经过我们)。在 QQ 音乐提交 timeline 之前强行写入 `MinSeekTime = 0`、`MaxSeekTime = 时长`,系统据此对外声明"可跳转"。

**第二步 —— 真正执行跳转。** 注册 `PlaybackPositionChangeRequested`,收到请求后不去点进度条,而是直接调 QQ 音乐自己的命令行命令处理函数:

```
QQMusic.dll + 0x5DF2F0
__thiscall HandleSeekTo(this, wchar_t** 秒数, 标志)

标志 != 0  →  内部只做一件事:取主窗口对象,SendMessage(0x272D, 0, 秒数字符串)
标志 == 0  →  另一条路(把命令回传给另一个实例),受闸门字段控制,不要用
```

关键发现是:**这条分支根本不碰 `this`**,实测传 `NULL` 照样生效。所以不需要抓任何对象、不需要进度条像素、不需要窗口可见。

值的单位是**整秒**(内部按 `_wtoi` 截断),超出时长会被当成 0,因此插件自己负责把目标夹到 `[0, 时长)`。

这个入口来自 QQ 音乐自己的命令协议。分发器在 `QQMusic.dll + 0x5DDD00`,是一条 `wcscmp` 长链,认得 `seekto / volumeto / forward / rewind / playmode / playUrl / startFrom / catplay / backToPlay / parsexmlcmdraw` —— 想做音量、切歌之类的扩展,从这张表里找。

> 上面这些偏移是 22.71 的地址。运行时真正用的是按特征定位的结果(见"QQ 音乐升级了怎么办"),偏移只作为定位失败时的兜底。

## 效果

QQ 音乐 22.71,Windows 11。跳转目标由外部 SMTC 客户端发起(`TryChangePlaybackPositionAsync`),回读用内存里的播放进度:

| 曲目类型 | 请求 | 实际落点 | 误差 |
|---|---|---|---|
| 本地 wav(1200s) | 300s | 300.1s | 64 ms |
| 本地 wav(300s) | 210s | 210.3s | 91 ms |
| 在线免费曲(230s) | 120s | 121.1s | 回读滞后 |
| 在线试听片段(60s) | 27s | 27.0s | 0 ms |

- 误差量级在**几十毫秒**,残留部分基本是回读采样延迟,不是跳偏。用"跳转后立刻 `TryPauseAsync` 再读"验证过,落点是 `12.00 / 20.00 / 33.00` 这种精确整秒。
- **窗口最小化、窗口完全隐藏(`SW_HIDE`,相当于关到托盘)都照常生效。** 主路径只要求 QQ 音乐进程活着且有歌载入,不要求窗口可见、置顶、拿到焦点。

## 安装

### 准备

- Windows,QQ 音乐 22.71(其他版本见下面"QQ 音乐升级了怎么办")
- 自己编译需要 Rust(稳定版即可)+ 一套带 C++ 工具链的 Visual Studio / Build Tools

不想编译的话,直接下载 [Releases](../../releases) 里的 `payload.dll`,按下面"手动安装"那一步放进目录即可。

### 编译

```powershell
cargo build --release --target i686-pc-windows-msvc
```

产物在 `target/i686-pc-windows-msvc/release/payload.dll`。QQ 音乐是 32 位进程,**必须是 i386**,装进 64 位 DLL 会让它直接起不来(安装脚本会校验 PE 头)。

> 编译报 `LNK1104: 无法打开文件 msvcrt.lib`,说明机器上被 rustc 挑中的那套 VS 没装 C++ 工具链。装一份带 "使用 C++ 的桌面开发" 的 Build Tools,并确认 `cl.exe` / `link.exe` 与 `LIB` / `INCLUDE` 指向同一套。本项目刻意**没有**在 `.cargo/config.toml` 里钉绝对路径 —— 那种写法换台机器就废了,只留了 `[build] target`。

### 部署

**最省事:双击 `plugin.bat`** —— 菜单选 安装 / 卸载 / 查看状态。它会自动申请管理员权限,也可以命令行调用:

```powershell
.\plugin.bat install      # 安装并重启 QQ 音乐
.\plugin.bat uninstall    # 卸载
.\plugin.bat check        # 只读:报告目录里现在装的是哪个插件、位数对不对
```

把 Releases 里下载的 `payload.dll` 放在 `plugin.bat` 同目录,它就会用这一份;否则用 `target\...` 下的编译产物。

> `plugin.bat` 刻意写成纯 ASCII:cmd.exe 按字节偏移续读 .bat,`chcp 65001` 下的多字节中文会让偏移错位,把下一行的残片当命令执行。中文提示都放在 `.ps1` 里(UTF-8 带 BOM,安全)。

或者直接用脚本:

```powershell
.\scripts\install.ps1 -Restart
```

脚本从注册表 `InstallLocation` 解析真实安装目录并写入。QQ 音乐自升级后真实目录**可能**是带版本号的一层,例如
`C:\Program Files (x86)\Tencent\QQMusic\QQMusic2271.13.31.33\`,而外层 `QQMusic\` 只剩上一版的残留文件 —— **必须装进当前真正在用的那个目录**,装错地方等于没装。脚本以注册表为准;拿不准就先跑 `.\plugin.bat check`,它会把解析到的目录和目录里的 `msimg32.dll` 来源一起报出来。

手动安装等价于:把 `payload.dll` 改名为 `msimg32.dll`,放进上面那个目录。

> 另一个判据:如果外层 `QQMusic\` 里**没有** `QQMusicAgent.exe`,说明外层只是升级残留、真正在用的是版本子目录;从外层启动会报"皮肤引擎初始化失败 InitPlatform faild"。

### 验证

```powershell
.\probe\smtc-probe.ps1                  # 列出会话、能力位、timeline(含 minSeek/maxSeek)
.\probe\smtc-probe.ps1 -SeekSeconds 75  # 跳到第 75 秒并回读
.\probe\smtc-probe.ps1 -Follow 10       # 每 500ms 采样一次进度
```

判定标准是 `-SeekSeconds` 那行:返回 `True` 且进度真的变了,就说明两步都通了。

### 卸载

```powershell
.\scripts\uninstall.ps1
```

只删我们自己写进去的 `msimg32.dll`(如果安装时备份过原始文件则还原备份)。

## 兜底路径

主路径(直调 `HandleSeekTo`)如果因为改版失效,调用会抛异常,被 SEH 捕获,然后自动退回**进程内模拟点击**:用 `PrintWindow` 抓自己的窗口、在底部找那条横线量出进度条端点、算出映射后点下去,必要时用前两次点击做标定。

这条兜底**依赖窗口可见和进度条几何**(沉浸大封面页和普通页的轨道位置不一样,最大化时又不一样),精度也只有约 1 像素对应的毫秒数。它只是保险,不是主路径。

## QQ 音乐升级了怎么办

`HandleSeekTo` 的入口**不是写死的**,运行期按特征定位:

1. 在已加载的镜像里找到 `/seekto '%s'` 这条 UTF-16 字面量(全镜像唯一),换算成**运行时地址**;
2. 搜 `68 <该地址>`(即 `push imm32`,全镜像只有一处);
3. 从它往前回找 MSVC 的 SEH 函数头 `55 8B EC 6A FF 68`。

只要那条格式串还在,客户端改版一般能自己跟上。定位失败会退回 `internal.rs` 里的 `FALLBACK_RVA`。

要手工重新定位:

```powershell
# 反汇编 QQMusic.dll 之后
python .\tools\pe.py str  <QQMusic.dll 路径> 0x...   # 看字符串
python .\tools\pe.py imports <dll 路径> <模块名>       # 看导入表和 IAT 地址
```

`tools/pe.py` 是个小工具:PE 导入表(带 IAT 虚拟地址)、VA↔字符串、VA↔字节。

## 诊断通道(默认全部关闭)

逆向过程中留下的探针,只在 `%TEMP%\QQMusicInjectorLogs\` 下出现对应 `.flag` 文件时才启用,平时零开销。

| 开关 | 作用 |
|---|---|
| `cmd-probe.flag` | 挂住命令分发器,起一个指令文件线程读 `cmd-probe.txt`。指令有 `direct:`(走生产代码)、`seek:` / `blind:` / `fwd:`(用不同参数形态直调 `HandleSeekTo`)、`msg:` / `msgs:`(绕过它直接 `SendMessage(0x272D)`)、`peek`(打印对象字段)。配套脚本 `.\scripts\cmd-probe.ps1 -Setup` / `-Seek <秒>` / `-Seek <秒> -Direct` |
| `trace.flag` | 给内存里的播放进度字段下 x86 硬件写断点(DR0 + VEH),记录每次写入的 EIP 和调用栈,25 秒后自动解除并删掉标志。就是靠它量出"进度字段其实是大对象的 `+0x1AC`" |

> 硬件断点这里有个坑值得记一下:x86 的 `DR7` 保留位(bit 8/10/12/13)必须置 1,否则 32 位 `NtSetContextThread` 会**静默丢弃** `DR7` 却仍然返回成功。监视 4 字节写入的正确值是 `0x000D3501`。

日志目录:`%TEMP%\QQMusicInjectorLogs\payload.<日期>.log`。

## 已知限制

- **整秒粒度。** `/seekto` 只接受整数秒。对 SMTC 的拖条来说够用(系统 UI 本身也是秒级显示)。
- **非 VIP 的每日免费试听额度用完后**,试听片段可能不再响应跳转,而且内存里读不到 `progress`(此时插件会判定"没到位"并尝试点击兜底)。额度用尽前,同一类 60s 试听片段是可以正常跳的 —— 这属于客户端状态,不是插件回归。要复验建议用本地文件。
- 只往安装目录写**一个**文件(`msimg32.dll`)。早期版本还会把 `SMTCFeature.dll` 换成代理,实测那层完全没必要,已经去掉;`uninstall.ps1` 仍会顺手还原历史版本留下的代理。
- 依赖 QQ 音乐进程在运行。它退出后能力自然消失。
- 只在本机的 22.71 上验证过。

## 免责声明

- 本项目**仅供个人学习与技术研究**。它修改腾讯客户端安装目录里的文件、并注入其进程,可能违反《QQ 音乐软件许可及服务协议》,**存在账号风险**,后果自负。
- 与腾讯公司无任何关联,不是官方产品。
- 不要用于商业分发、批量部署或任何绕过付费/版权保护的目的。
- 出问题先用 `uninstall.ps1` 还原;需要重装客户端时,安装目录里我们写的那个文件删掉即可。

## 致谢

- [apoint123/QQMusic-ID-Injector](https://github.com/apoint123/QQMusic-ID-Injector) —— 本项目的代码基线(DLL 劫持骨架、`CurrentSongInfo` 特征码、ID 流派字段与高清封面)。原始许可见 `LICENSE.upstream`。
- [HChenX/BodianSMTCPlugin](https://github.com/HChenX/BodianSMTCPlugin) —— 波点音乐的同类插件,证实了"注入进程内复用自身 SMTC 实例 + 补 `Min/MaxSeekTime`"这条路走得通。

## 许可

MIT,见 `LICENSE`。包含来自上游的 MIT 代码,故一并保留 `LICENSE.upstream`。
