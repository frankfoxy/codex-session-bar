# 发布版本

Git 标签标记源码版本；GitHub Release 提供该版本的说明和可下载程序。当前版本为 **0.2.0**，对应标签 **v0.2.0**。

## 1. 准备与打包

在仓库根目录执行。需要 Rust MSVC 工具链和 Visual Studio C++ Build Tools。

1. 修改 `Cargo.toml` 中的包版本，并同步 `Cargo.lock` 中 `codex-session-bar` 的版本；填写 `CHANGELOG.md`。
2. 运行下列命令。脚本先测试、构建，再生成 Windows x64 ZIP 和 `SHA256SUMS.txt`，输出位于工作区 `tmp`。构建不会覆盖正在运行的安装。

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\Package.ps1
```

3. 解压 ZIP，确认 `bin\codex-session-bar.exe --version` 与目标版本一致，运行 `--self-test` 并检查生成的演示图。打包使用固定清单，不包含个人设置、会话数据、凭据或运行日志。
4. 提交并推送已验证的源码与说明。

## 2. 标签与 GitHub Release

以当前版本为例：

```powershell
git tag -a v0.2.0 -m "Codex Session Bar v0.2.0"
git push origin v0.2.0
```

打开仓库的 **Releases → Draft a new release**，选择 `v0.2.0`，标题填写 `Codex Session Bar v0.2.0`。填写变更说明和运行要求，附加打包脚本输出的 ZIP 与 `SHA256SUMS.txt`，确认后点击 **Publish release**。

已安装并登录 GitHub CLI 时，也可使用下列命令；将占位路径替换为打包输出的实际文件，发布说明文件放在工作区 `tmp`：

```powershell
gh release create v0.2.0 <ZIP路径> <SHA256SUMS.txt路径> --verify-tag --title "Codex Session Bar v0.2.0" --notes-file <发布说明路径>
```

后续版本递增版本号并创建新标签。保留已发布标签，不覆盖旧版本附件。

## 下载与升级

用户下载 **windows-x64.zip**，解压到固定目录，打开 Codex 后双击 `Launch.cmd`。无需 Rust、Python 或 Qt；运行要求见 [README](README.md)。GitHub 自动提供的 Source code ZIP 是源码包，需要自行构建。

升级前从托盘退出旧程序，再替换文件。若更换安装目录，重新运行 `Enable-Autostart.ps1`，使启动任务指向新位置。

参考：[GitHub 发布说明](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository)、[GitHub CLI release create](https://cli.github.com/manual/gh_release_create)。
