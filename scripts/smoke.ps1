<#
.SYNOPSIS
    Pixora 端到端冒烟测试（Windows / PowerShell 版）

.DESCRIPTION
    起一个真服务，按真实调用顺序走完整条主链路：安装向导 → 登录 → 建相册 →
    上传 → 去重 → 直链 → 可见性 → 删除 → 回收物理文件。

    为什么在 Windows 上单独写一份而不是让大家去调 bash：Git Bash 里跑这个流程
    要先解决 MSYS 的路径转换（/tmp 在 bash、Python、curl 三处指向不同位置），
    命令行里的中文还会被按本地代码页编码、到服务端就成了非法 UTF-8。这份脚本
    全部用文件承载请求体（curl 的 @file / <file 写法），不把中文放进参数里。

    依赖：Windows 10 自带的 curl.exe、PowerShell 5.1（都无需安装）。
    断言与 scripts/smoke.sh 对齐；两者覆盖同一批行为，选哪个取决于你手边是什么 shell。

    文件编码必须是 UTF-8 with BOM：Windows PowerShell 5.1 在没有 BOM 时按本地
    代码页读脚本，中文注释和字符串会全部乱码并直接报语法错。用 PowerShell 7
    或改这个文件时，注意别把 BOM 丢掉。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1
    powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1 -Port 19000
#>
[CmdletBinding()]
param(
    # 服务监听端口
    [int]$Port = 18080,
    # 服务二进制路径（默认用 debug 构建产物）
    [string]$ServerExe = "target\debug\pixora-server.exe",
    # 测试结束后保留临时目录，便于排查失败原因
    [switch]$KeepTemp
)

$ErrorActionPreference = "Stop"

# 关键一步：PowerShell 5.1 默认按本地代码页（简体中文系统上是 GBK）解码原生命令的
# 输出。curl.exe 拿回来的是 UTF-8 JSON，被按 GBK 解一遍就成乱码，ConvertFrom-Json
# 直接失败。这里显式改成 UTF-8。
$script:prevOutputEncoding = [Console]::OutputEncoding
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

# ---------- 输出 ----------
function Step($text) { Write-Host "`n== $text" -ForegroundColor Cyan }
function Ok($text) { Write-Host "  [OK] $text" -ForegroundColor Green }
function Fail($text) {
    Write-Host "  [FAIL] $text" -ForegroundColor Red
    if ($script:LogPath -and (Test-Path $script:LogPath)) {
        Write-Host "--- 服务端日志尾部 ---" -ForegroundColor DarkGray
        Get-Content $script:LogPath -Tail 40 -Encoding UTF8 | ForEach-Object { Write-Host $_ }
    }
    throw "冒烟测试失败：$text"
}

# ---------- 工具 ----------
# 注意：请求函数不能叫 Curl。PowerShell 的命令解析顺序是「别名 > 函数」，
# 而 Windows PowerShell 内置了别名 curl -> Invoke-WebRequest，同名函数会被它抢走，
# 报错长这样：无法将 System.Object[] 转换为参数 Uri 所需的类型。所以叫 HttpText。

# 写 UTF-8（不带 BOM）的文件。BOM 会让 JSON 解析失败，中文也必须按 UTF-8 落盘。
function Write-Utf8([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}

# 把 JSON 体写进文件，返回 curl 需要的 @path 形式
function JsonBody([string]$Name, $Object) {
    $file = Join-Path $script:Work "$Name.json"
    Write-Utf8 $file ($Object | ConvertTo-Json -Compress -Depth 6)
    return "@$file"
}

# 表单文本字段：写文件后用 curl 的 <file 写法读值，绕开命令行编码问题
function FormValue([string]$Name, [string]$Text) {
    $file = Join-Path $script:Work "field-$Name.txt"
    Write-Utf8 $file $Text
    return "<$file"
}

# 发一个请求，返回响应文本
function HttpText([string[]]$Arguments) {
    $output = & curl.exe -s @Arguments
    if ($LASTEXITCODE -ne 0) { throw "curl 退出码 $LASTEXITCODE" }
    return ($output -join "`n")
}

# 带会话与 CSRF 的请求（写操作）
function Api([string]$Method, [string]$Path, [string[]]$Extra = @()) {
    $args = @("-X", $Method, "$script:Base$Path", "-b", $script:Jar, "-c", $script:Jar)
    if ($Method -ne "GET") { $args += @("-H", "X-CSRF-Token: $script:Csrf") }
    $args += $Extra
    return HttpText $args
}

# 断言 JSON 字段。$Script 块里用 $d 引用解析后的响应对象。
function ExpectJson([string]$Json, [scriptblock]$Check, [string]$Label) {
    try {
        $d = $Json | ConvertFrom-Json
    } catch {
        Write-Host "  响应原文：$Json" -ForegroundColor DarkGray
        Fail "$Label：响应不是合法 JSON"
    }
    $actual = & $Check
    if (-not $actual) {
        Write-Host "  响应原文：$Json" -ForegroundColor DarkGray
        Fail "$Label"
    }
    Ok $Label
}

function ExpectStatus([string]$Url, [int]$Code, [string]$Label) {
    $code = & curl.exe -s -o NUL -w "%{http_code}" $Url
    if ($code -ne "$Code") { Fail "$Label：期望 HTTP $Code，实际 $code" }
    Ok "$Label（HTTP $Code）"
}

# ---------- 准备 ----------
$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot

$exe = if (Test-Path $ServerExe) { (Resolve-Path $ServerExe).Path } else { $null }
if (-not $exe) {
    Write-Host "找不到 $ServerExe。先构建：" -ForegroundColor Yellow
    Write-Host "  cargo build -p pixora-server"
    Write-Host ""
    Pop-Location
    exit 2
}

$work = Join-Path $env:TEMP ("pixora-smoke-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
$dataDir = Join-Path $work "data"
$dbPath = Join-Path $work "pixora.db"
$log = Join-Path $work "server.log"
$errLog = Join-Path $work "server.err.log"
$jar = Join-Path $work "cookies.txt"
$png = Join-Path $work "sample.png"
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null

$script:Work = $work
$script:LogPath = $log
$script:Jar = $jar
$script:Base = "http://127.0.0.1:$Port"
$script:Csrf = ""

$server = $null
function Cleanup {
    if ($script:server -and -not $script:server.HasExited) {
        Stop-Process -Id $script:server.Id -Force -ErrorAction SilentlyContinue
    }
    Pop-Location -ErrorAction SilentlyContinue
    if ($script:prevOutputEncoding) { [Console]::OutputEncoding = $script:prevOutputEncoding }
    if (-not $KeepTemp -and (Test-Path $work)) {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    } elseif ($KeepTemp) {
        Write-Host "临时目录保留在 $work" -ForegroundColor DarkGray
    }
}

try {
    Step "启动服务"
    # 环境变量要为子进程准备好：PowerShell 5.1 的 Start-Process 不支持 -Environment，
    # 所以先在当前进程设好，Start-Process 会继承。
    $env:PIXORA_DB_DRIVER = "sqlite"
    $env:PIXORA_DB_DSN = $dbPath
    $env:PIXORA_DATA_DIR = $dataDir
    $env:PIXORA_BIND = "127.0.0.1:$Port"
    $env:PIXORA_BASE_URL = "http://127.0.0.1:$Port"
    $env:PIXORA_SECURE_COOKIES = "false"
    $env:RUST_LOG = "info"

    $script:server = Start-Process -FilePath $exe -PassThru -NoNewWindow `
        -RedirectStandardOutput $log -RedirectStandardError $errLog

    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Seconds 1
        if ($script:server.HasExited) { Fail "服务进程提前退出（退出码 $($script:server.ExitCode)）" }
        $code = & curl.exe -s -o NUL -w "%{http_code}" "$script:Base/healthz" 2>$null
        if ($code -eq "200") { $ready = $true; break }
    }
    if (-not $ready) { Fail "健康检查超时" }
    Ok "服务已就绪 $script:Base"

    Step "静态资源与前端回落"
    $index = HttpText @("$script:Base/")
    if ($index -notmatch 'id="app"') { Fail "首页没有返回前端外壳" }
    Ok "首页返回了 SPA 外壳"

    $jsBytes = & curl.exe -s -o (Join-Path $work "main.js") -w "%{size_download}" "$script:Base/main.js"
    if ([int]$jsBytes -lt 1000) { Fail "main.js 只有 $jsBytes 字节" }
    Ok "前端脚本可访问（$jsBytes 字节）"

    ExpectStatus "$script:Base/main.css" 200 "样式表可访问"
    ExpectStatus "$script:Base/albums" 200 "SPA 路由回落到 index.html"

    # 产物是固定文件名，不能长缓存：no-cache + ETag，第二次应当拿 304
    # 用 curl 的 --etag-save/--etag-compare：PowerShell 5.1 给原生命令传内嵌引号
    # 时不做转义（If-None-Match 的值本身必须带引号），走命令行参数必踩坑
    $etagFile = Join-Path $work "main.js.etag"
    & curl.exe -s -o NUL --etag-save $etagFile "$script:Base/main.js" | Out-Null
    if (-not (Test-Path $etagFile)) { Fail "main.js 没有 ETag" }
    $cacheCode = (& curl.exe -s -o NUL -w "%{http_code}" --etag-compare $etagFile "$script:Base/main.js")
    if ($cacheCode -ne "304") { Fail "带 ETag 再取 main.js 返回了 $cacheCode" }
    Ok "静态资源带 ETag，重复请求拿 304"

    Step "安装向导"
    $status = HttpText @("$script:Base/api/v1/install/status")
    ExpectJson $status { -not $d.data.installed } "未安装状态"

    # 用正斜杠路径：SQLite 的 DSN 里反斜杠会被当成转义
    $dbForApi = $dbPath -replace '\\', '/'
    $check = HttpText @("-X", "POST", "$script:Base/api/v1/install/database", "-H", "Content-Type: application/json",
        "-d", (JsonBody "db" @{ driver = "sqlite"; database = $dbForApi }))
    ExpectJson $check { $d.data.ok } "数据库连接与建表"

    $admin = HttpText @("-X", "POST", "$script:Base/api/v1/install/admin", "-H", "Content-Type: application/json",
        "-d", (JsonBody "admin" @{ email = "admin@example.com"; password = "Pixora2026Abc"; site_name = "Pixora" }))
    ExpectJson $admin { $d.data.email -eq "admin@example.com" } "创建管理员"

    $site = HttpText @("-X", "POST", "$script:Base/api/v1/install/site", "-H", "Content-Type: application/json",
        "-d", (JsonBody "site" @{ base_url = $script:Base; upload_prefix = "images"; smtp = $null }))
    ExpectJson $site { $d.data.ok } "站点设置"

    $finish = HttpText @("-X", "POST", "$script:Base/api/v1/install/finish")
    ExpectJson $finish { $d.data.ok } "完成安装"
    ExpectStatus "$script:Base/api/v1/install/status" 404 "安装入口已锁定"

    Step "登录与会话"
    $badLogin = HttpText @("-X", "POST", "$script:Base/api/v1/auth/login", "-H", "Content-Type: application/json",
        "-d", (JsonBody "badlogin" @{ email = "admin@example.com"; password = "wrong-password" }))
    ExpectJson $badLogin { $d.error.code -eq "INVALID_CREDENTIALS" } "错误口令被拒绝"

    $login = HttpText @("-X", "POST", "$script:Base/api/v1/auth/login", "-H", "Content-Type: application/json",
        "-b", $jar, "-c", $jar,
        "-d", (JsonBody "login" @{ email = "admin@example.com"; password = "Pixora2026Abc" }))
    ExpectJson $login { $d.data.user.email -eq "admin@example.com" } "登录成功"

    $script:Csrf = ($login | ConvertFrom-Json).data.csrf_token
    if ([string]::IsNullOrWhiteSpace($script:Csrf)) { Fail "登录未返回 CSRF 令牌" }
    Ok "拿到 CSRF 令牌"

    ExpectJson (Api GET "/api/v1/me") { $d.data.user.role -eq "admin" } "会话鉴权可用"

    # 演示 CSRF 缺失：不带 X-CSRF-Token 的写请求必须被拒
    $noCsrf = & curl.exe -s -o NUL -w "%{http_code}" -X POST "$script:Base/api/v1/albums" `
        -b $jar -H "Content-Type: application/json" -d (JsonBody "noCsrf" @{ name = "x" })
    if ($noCsrf -ne "403") { Fail "缺 CSRF 头的写请求未被拒绝（$noCsrf）" }
    Ok "缺 CSRF 的写请求返回 403"

    Step "相册与图片"
    $albumJson = Api POST "/api/v1/albums" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "album" @{ name = "旅行"; slug = "旅行"; visibility = "private" }))
    ExpectJson $albumJson { $d.data.name -eq "旅行" } "新建相册（中文名与 slug）"
    $albumId = ($albumJson | ConvertFrom-Json).data.id

    # 一张 96×96 的纯色 PNG（221 字节），内联在脚本里，不依赖任何图形库
    $pngBase64 = "iVBORw0KGgoAAAANSUhEUgAAAGAAAABgCAIAAABt+uBvAAAApElEQVR4nO3QQQ0AIBDAsNOJJiQiBgd82aPJBCydtY8ezfeDeIAAAQIEKBwgQIAAAQoHCBAgQIDCAQIECBCgcIAAAQIEKBwgQIAAAQoHCBAgQIDCAQIECBCgcIAAAQIEKBwgQIAAAQoHCBAgQIDCAQIECBCgcIAAAQIEKBwgQIAAAQoHCBAgQIDCAQIECBCgcIAAAQIEKBwgQIAAAQoHCBAgQD+7mtxLsldE+bcAAAAASUVORK5CYII="
    [System.IO.File]::WriteAllBytes($png, [Convert]::FromBase64String($pngBase64))
    if ((Get-Item $png).Length -ne 221) { Fail "测试图片生成失败" }
    $hash = (Get-FileHash -Algorithm SHA256 $png).Hash.ToLower()
    Ok "生成测试 PNG（96×96，哈希 $($hash.Substring(0,8))…）"

    $pre = Api POST "/api/v1/blobs/exists" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "pre" @{ hashes = @($hash) }))
    ExpectJson $pre { @($d.data.missing).Count -eq 1 } "预检报告文件不存在"

    $upload = Api POST "/api/v1/upload" @(
        "-F", "file=@$($png -replace '\\','/')",
        "-F", "sha256=$hash",
        "-F", "album_id=$albumId",
        "-F", "tags=$(FormValue "tags" "风景,旅行")",
        "-F", "visibility=public",
        "-F", "original_name=sample.png"
    )
    ExpectJson $upload { -not $d.data.dedup } "首次上传未命中重复"
    ExpectJson $upload { $d.data.sha256 -eq $hash } "服务端哈希与客户端一致"
    $imageId = ($upload | ConvertFrom-Json).data.id
    $url = ($upload | ConvertFrom-Json).data.url

    $pre2 = Api POST "/api/v1/blobs/exists" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "pre2" @{ hashes = @($hash) }))
    ExpectJson $pre2 { @($d.data.exist).Count -eq 1 } "预检报告文件已存在"

    Step "去重与引用计数"
    # 第二次故意不带 album_id：这张落在「未分类」，用来验证相册计数只算自己那份
    $upload2 = Api POST "/api/v1/upload" @(
        "-F", "file=@$($png -replace '\\','/')", "-F", "sha256=$hash",
        "-F", "original_name=sample-again.png"
    )
    ExpectJson $upload2 { $d.data.dedup } "二次上传命中重复且未重复占盘"
    $imageId2 = ($upload2 | ConvertFrom-Json).data.id
    if ($imageId -eq $imageId2) { Fail "两次上传应当生成两条独立的图片引用" }
    Ok "两次上传生成了两条独立引用（同一物理文件）"

    $blobCount = @(Get-ChildItem -Recurse -File (Join-Path $dataDir "blobs")).Count
    if ($blobCount -ne 1) { Fail "物理文件数应为 1，实际 $blobCount" }
    Ok "磁盘上只有 1 份物理文件"

    $viewLinks = @(Get-ChildItem -Recurse -File (Join-Path $dataDir "users") -ErrorAction SilentlyContinue).Count
    if ($viewLinks -lt 1) { Fail "镜像目录树里没有链接" }
    Ok "镜像目录树里建了 $viewLinks 个链接"

    Step "直链"
    $urlPath = $url.Replace($script:Base, "")
    ExpectStatus "$script:Base$urlPath" 200 "直链返回 200"

    $headers = HttpText @("-D", "-", "-o", "NUL", "$script:Base$urlPath")
    if ($headers -notmatch "immutable") { Fail "直链缺少 immutable 缓存头" }
    if ($headers -notmatch "nosniff") { Fail "直链缺少 nosniff" }
    if ($headers -notmatch $hash) { Fail "直链 ETag 不等于内容哈希" }
    Ok "直链响应头正确（immutable / nosniff / ETag）"

    $downloaded = Join-Path $work "downloaded.png"
    & curl.exe -s -o $downloaded "$script:Base$urlPath" | Out-Null
    $downHash = (Get-FileHash -Algorithm SHA256 $downloaded).Hash.ToLower()
    if ($downHash -ne $hash) { Fail "直链返回的字节与原文件不一致" }
    Ok "直链内容与上传的字节完全一致"

    $wrongExt = [System.IO.Path]::ChangeExtension($urlPath, ".gif")
    ExpectStatus "$script:Base$wrongExt" 404 "伪造扩展名返回 404"

    Step "列表与广场"
    $images = Api GET "/api/v1/images?album_id=$albumId"
    ExpectJson $images { $d.meta.total -eq 1 } "相册里有 1 张图"
    ExpectJson $images { @($d.data[0].tags).Count -eq 2 } "标签已写入"

    $allImages = Api GET "/api/v1/images"
    ExpectJson $allImages { $d.meta.total -eq 2 } "全部图片共 2 张"

    $plaza = HttpText @("$script:Base/api/v1/plaza")
    ExpectJson $plaza { @($d.data).Count -ge 1 } "广场能看到公开图片"
    ExpectJson $plaza { $null -eq $d.data[0].original_name } "未登录时原始文件名被裁剪"

    $tags = Api GET "/api/v1/tags?uid=1000"
    ExpectJson $tags { @($d.data).Count -ge 1 } "标签词表"

    # 随机图：默认 302 跳直链，?format=json 给元数据，挑不到就 404
    $randomCode = (& curl.exe -s -o NUL -w "%{http_code}" "$script:Base/api/v1/random")
    if ($randomCode -ne "302") { Fail "随机图没有跳转（$randomCode）" }
    $randomLoc = (& curl.exe -s -o NUL -w "%{redirect_url}" "$script:Base/api/v1/random")
    if ($randomLoc -notmatch "/images/") { Fail "随机图跳转地址不对：$randomLoc" }
    Ok "随机图 302 到直链"

    $randomJson = HttpText @("$script:Base/api/v1/random?format=json")
    ExpectJson $randomJson { $d.data.ext -eq "png" } "随机图 JSON 带扩展名"
    $randomFile = Join-Path $work "random.png"
    $randomFollow = (& curl.exe -s -L -o $randomFile -w "%{http_code}" "$script:Base/api/v1/random")
    if ($randomFollow -ne "200") { Fail "跟随随机图跳转没拿到图片（$randomFollow）" }
    if ((Get-Item $randomFile).Length -le 100) { Fail "随机图拿到的是空文件" }
    Ok "随机图跟随跳转拿到图片"

    $randomMiss = (& curl.exe -s -o NUL -w "%{http_code}" "$script:Base/api/v1/random?q=zzz_no_such_keyword")
    if ($randomMiss -ne "404") { Fail "随机图挑不到图片时返回了 $randomMiss" }
    Ok "随机图挑不到图片时返回 404"

    $siteInfo = HttpText @("$script:Base/api/v1/site")
    ExpectJson $siteInfo { $d.data.plaza_grid_columns -eq 3 } "站点信息带广场列数（默认 3）"


    $tree = Api GET "/api/v1/fs/tree"
    ExpectJson $tree { $d.data.files -eq 2 } "目录树统计到 2 个文件"

    Step "可见性、批量与删除"
    Api PATCH "/api/v1/images/$imageId" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "visprivate" @{ visibility = "private" })) | Out-Null
    ExpectStatus "$script:Base$urlPath" 404 "全部引用转私有后直链返回 404"

    Api PATCH "/api/v1/images/$imageId" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "vispublic" @{ visibility = "public" })) | Out-Null
    ExpectStatus "$script:Base$urlPath" 200 "恢复公开后直链可用"

    $batch = Api POST "/api/v1/images/batch" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "batchtag" @{ action = "tag"; ids = @($imageId, $imageId2); tags = @("风景") }))
    ExpectJson $batch { $d.data.affected -eq 2 } "批量改标签"

    Api DELETE "/api/v1/images/$imageId2" | Out-Null
    $left = @(Get-ChildItem -Recurse -File (Join-Path $dataDir "blobs")).Count
    if ($left -ne 1) { Fail "删掉一条引用后物理文件不该消失（实际 $left）" }
    Ok "删掉一条引用后物理文件保留（还有另一条引用，回收站也留着它）"

    Api DELETE "/api/v1/images/$imageId" | Out-Null
    ExpectStatus "$script:Base$urlPath" 404 "全部引用删除后直链返回 404"

    # 回收站：删掉的图先到这里；恢复之后回到相册；彻底删除之后才真没了
    $bin = Api GET "/api/v1/recycle/images"
    ExpectJson $bin { $d.meta.total -eq 2 } "两张删掉的图都在回收站里"
    ExpectJson $bin { $d.data[0].size -gt 0 } "回收站列表带完整字段"

    $restoreOne = Api POST "/api/v1/recycle/images/restore" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "binrestore" @{ ids = @($imageId) }))
    ExpectJson $restoreOne { $d.data.restored -eq 1 } "从回收站恢复 1 张"
    $alive = Api GET "/api/v1/images?album_id=$albumId"
    ExpectJson $alive { $d.meta.total -eq 1 } "恢复之后图片回到相册里"
    ExpectStatus "$script:Base$urlPath" 200 "恢复之后直链又能访问"

    Api DELETE "/api/v1/images/$imageId" | Out-Null
    $purged = Api POST "/api/v1/recycle/images/purge" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "binpurge" @{ ids = @($imageId) }))
    ExpectJson $purged { $d.data.purged -eq 1 } "彻底删除 1 张"
    $gone = Api GET "/api/v1/images/$imageId"
    ExpectJson $gone { $d.error.code -eq "NOT_FOUND" } "彻底删除之后查不到这张图"
    $binOne = Api GET "/api/v1/recycle/images"
    ExpectJson $binOne { $d.meta.total -eq 1 } "回收站里只剩另一张"

    # 删光之后重传同一张图：预检报「不存在」，上传把图带回来
    $remiss = Api POST "/api/v1/blobs/exists" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "preflight2" @{ hashes = @($hash) }))
    ExpectJson $remiss { @($d.data.exist).Count -eq 0 } "删光后预检不再报「已存在」"

    $reupload = Api POST "/api/v1/upload" @("-F", "file=@$png", "-F", "sha256=$hash",
        "-F", "album_id=$albumId", "-F", "visibility=public")
    ExpectJson $reupload { $d.data.dedup } "重传命中服务端去重（不重复占盘）"
    $reuploadId = ($reupload | ConvertFrom-Json).data.id
    if ($reuploadId -eq $imageId) { Fail "重传应当是新的图片记录" }
    Ok "删光后重传真的把图带回来了"

    # 删相册 = 相册连图一起进回收站：镜像目录收掉，但整份可以恢复
    $albumDir = Get-ChildItem (Join-Path $dataDir "users") -Recurse -Directory -Filter "旅行*" |
        Select-Object -First 1
    if (-not $albumDir) { Fail "镜像树里没有相册目录" }
    Api DELETE "/api/v1/albums/$albumId" | Out-Null
    if (Test-Path $albumDir.FullName) { Fail "进回收站后镜像目录还在：$($albumDir.FullName)" }
    Ok "删相册后镜像目录已收掉"
    $liveAfter = Api GET "/api/v1/images"
    ExpectJson $liveAfter { $d.meta.total -eq 0 } "相册里的图跟着进了回收站，不在正常列表里"
    $binAlbums = Api GET "/api/v1/recycle/albums"
    ExpectJson $binAlbums { @($d.data.albums).Count -eq 1 } "回收站里能看到这个相册"

    $restoreAlbum = Api POST "/api/v1/recycle/albums/$albumId/restore"
    ExpectJson $restoreAlbum { $d.data.restored_images -eq 1 } "恢复相册把图一起带回来"
    $liveBack = Api GET "/api/v1/images?album_id=$albumId"
    ExpectJson $liveBack { $d.meta.total -eq 1 } "图片回到相册里"
    if (-not (Test-Path $albumDir.FullName)) { Fail "恢复之后镜像目录应当重新出现" }
    Ok "恢复之后镜像目录也回来了"

    # 再删一次、这次彻底删除
    Api DELETE "/api/v1/albums/$albumId" | Out-Null
    $purgeAlbum = Api DELETE "/api/v1/recycle/albums/$albumId"
    ExpectJson $purgeAlbum { $d.data.purged_images -eq 1 } "彻底删除相册带走了 1 张图"
    $binFinal = Api GET "/api/v1/recycle/images"
    ExpectJson $binFinal { $d.meta.total -eq 1 } "回收站里只剩早先那张（未随相册删除）"
    $emptyBin = Api POST "/api/v1/recycle/images/empty"
    ExpectJson $emptyBin { $d.data.purged -eq 1 } "清空回收站"
    $binEmpty = Api GET "/api/v1/recycle/images"
    ExpectJson $binEmpty { $d.meta.total -eq 0 } "回收站已经空了"

    # 引用归零 ≠ 立刻回收：物理文件由后台 GC 在宽限期之后删，回收行为由
    # crates/pixora-queue/tests/gc.rs 覆盖（含「回收站保留期内绝不删」）。
    $stillThere = @(Get-ChildItem -Recurse -File (Join-Path $dataDir "blobs")).Count
    if ($stillThere -lt 1) { Fail "宽限期内物理文件不该被删（实际 $stillThere 个）" }
    Ok "引用归零，物理文件留待后台 GC 回收"

    Step "管理后台"
    ExpectJson (Api GET "/api/v1/admin/stats") { $d.data.user_total -ge 1 } "管理概览"
    ExpectJson (Api GET "/api/v1/admin/storage") { $d.data.default_driver -eq "local" } "存储总览：默认驱动"
    ExpectJson (Api GET "/api/v1/admin/storage") { $d.data.paths.upload_prefix -eq "images" } "存储总览：直链前缀"
    ExpectJson (Api GET "/api/v1/admin/quota") { $d.data.default_quota_bytes -gt 0 } "配额总览"
    # 容量自动探测：Windows 走 Win32 盘类型查询（Linux 走 statfs），
    # 来源降级或总额为 0/null，说明当前平台的探测路径断了。
    ExpectJson (Api GET "/api/v1/admin/quota") { $d.data.capacity.source -eq "auto" } "容量自动探测：来源是 auto"
    ExpectJson (Api GET "/api/v1/admin/quota") { $d.data.capacity.total -gt 0 } "容量自动探测：系统总额 > 0"
    # 宿主机整盘已用：自动模式下必须给出来（占用率与自动关闭注册按它算），
    # 且已用 + 可用 = 总额，两个数来自同一次探测
    ExpectJson (Api GET "/api/v1/admin/quota") { $d.data.capacity.host_used -gt 0 } "容量自动探测：宿主机整盘已用 > 0"
    ExpectJson (Api GET "/api/v1/admin/quota") { ($d.data.capacity.host_used + $d.data.capacity.host_available) -eq $d.data.capacity.total } "整盘口径：已用 + 可用 = 总额"
    # 可分配上限 = 总额 − 系统占用：配额校验与分配进度都按它算
    ExpectJson (Api GET "/api/v1/admin/quota") { $d.data.allocatable -eq ($d.data.capacity.total - $d.data.capacity.host_used) } "可分配上限 = 总额 − 系统占用"
    ExpectJson (Api GET "/api/v1/admin/settings") { $d.data.site.name -eq "Pixora" } "系统设置：站点名"

    Api PUT "/api/v1/admin/storage/prefix" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "prefix" @{ upload_prefix = "i" })) | Out-Null
    Ok "直链前缀改为 i"

    # 后台建号 → 新账号能登录 → 提权 → 降级 → 删除
    $newUser = Api POST "/api/v1/admin/users" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "newuser" @{ email = "smoke-user@example.com"; password = "SmokeUser2026Abc"; role = "user" }))
    ExpectJson $newUser { $d.data.user.email -eq "smoke-user@example.com" } "后台建号：邮箱"
    $newId = ($newUser | ConvertFrom-Json).data.user.id

    # 新账号登录用独立请求：不能借用管理员那个 cookie jar，会把它覆盖掉
    $newLogin = HttpText @("-X", "POST", "$script:Base/api/v1/auth/login", "-H", "Content-Type: application/json",
        "-d", (JsonBody "newlogin" @{ email = "smoke-user@example.com"; password = "SmokeUser2026Abc" }))
    ExpectJson $newLogin { $d.data.user.email -eq "smoke-user@example.com" } "新账号能登录（邮箱已激活）"

    $promoted = Api PATCH "/api/v1/admin/users/$newId" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "promote" @{ role = "admin" }))
    ExpectJson $promoted { $d.data.user.role -eq "admin" } "普通用户提为管理员"

    $demoted = Api PATCH "/api/v1/admin/users/$newId" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "demote" @{ role = "user" }))
    ExpectJson $demoted { $d.data.user.role -eq "user" } "管理员降回普通用户"

    Api DELETE "/api/v1/admin/users/$newId" | Out-Null
    $rowsAfter = Api GET "/api/v1/admin/users"
    ExpectJson $rowsAfter { $d.meta.total -eq 1 } "测试账号已删除，用户数回到 1"

    ExpectJson (Api DELETE "/api/v1/admin/terms/99999") { $d.error.code -eq "NOT_FOUND" } "删除不存在的条款返回 NOT_FOUND"

    Step "错误码"
    ExpectJson (Api GET "/api/v1/images/999999") { $d.error.code -eq "NOT_FOUND" } "不存在的图片返回 NOT_FOUND"
    ExpectJson (HttpText @("$script:Base/api/v1/me", "-H", "Authorization: Bearer px_live_not_a_real_token")) `
        { $d.error.code -eq "TOKEN_REVOKED" } "伪造 Token 返回 TOKEN_REVOKED"

    $suspend = Api POST "/api/v1/admin/users/1/status" @("-H", "Content-Type: application/json",
        "-d", (JsonBody "suspend" @{ action = "suspend"; reason = "测试用" }))
    ExpectJson $suspend { $d.error.code -eq "BAD_REQUEST" } "管理员账号不可被处置"

    Step "审计与统计"
    $stats = & curl.exe -s "$script:Base/api/v1/admin/stats" -b $jar | ConvertFrom-Json
    if ($null -eq $stats.data) { Fail "管理统计拿不到" }
    Ok "管理统计可读（用户 $($stats.data.user_total) 个）"

    $audit = & curl.exe -s "$script:Base/api/v1/admin/ratelimit/logs" -b $jar | ConvertFrom-Json
    if ($null -eq $audit.data) { Fail "限流日志接口不可用" }
    Ok "限流日志接口可读（$(@($audit.data).Count) 条）"

    # 审计留痕：上传、删除、改前缀这些动作都应当写进审计表
    $storage = & curl.exe -s "$script:Base/api/v1/admin/storage" -b $jar | ConvertFrom-Json
    $totalImages = $storage.data.image_total
    if ($null -eq $totalImages) { Fail "存储总览缺少 image_total" }
    Ok "存储总览可读（图片总数 $totalImages）"

    Write-Host "`n全部通过。" -ForegroundColor Green
    exit 0
} catch {
    Write-Host "`n$_" -ForegroundColor Red
    exit 1
} finally {
    Cleanup
}
