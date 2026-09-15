#!/bin/sh
# ---------------------------------------------------------------------------
# install-local.sh —— 在本机（本机即设备）安装并启动 HiCodeer
#
# 不经过 VM、不需要 USB。链路只有一条：
#
#     hdc 客户端  ──TCP 回环──▶  本机 hdcd 监听的调试端口
#
# 连 hdc server 的步骤（本脚本前 4 步做的就是这件事）：
#   1. 选一条能用的 server 通道，优先级：
#        a) OHOS_HDC_SERVER_PORT 指定的端口（默认 18710）；
#        b) 不设该变量，走 hdc 的默认通道；
#        c) 都不通时才尝试拉起一个新 server。
#      OHOS_HDC_SERVER_PORT 只是「优先连这个 TCP server」的意愿：该端口没有
#      server 在监听时，客户端会安静地回退到编译期写死的 UDS
#      /data/hdc/hdc_debug/hdc_server。本机实测（2026-09-15）就是这种情况：
#      18710 上根本没有 server，真正干活的是 UDS 上那个常驻
#      `hdc -m -s uds`，所以选到 a 也照常能装。
#      「c 拉不起来」是常态而不是异常：普通 shell 无权 bind 那个 UDS，日志会报
#      'bind uds addr fail! ret:-13'。所以只要 a / b 之一能通就继续，
#      不要把「拉不起新 server」当成致命错误。
#   2. 复用已存在的 server，不预先 kill。旧 server 里残留的重复 target 用
#      tconn ... -remove 清掉，比换端口更稳。
#   3. tconn 127.0.0.1:<设备调试端口>，把本机登记成一个 target。
#   4. list targets 校验：出现设备条目即连接成功。
#
# 用法：
#   ./install-local.sh                 自动探测端口，装最新 HAP 并启动应用
#   ./install-local.sh --port 45661    手动指定设备调试端口
#   ./install-local.sh --server-port N 手动指定 hdc server 端口（默认 18710）
#   ./install-local.sh --hap <path>    指定要安装的 HAP
#   ./install-local.sh --reinstall     先卸载再安装（用于让 hicodeerd 换新版）
#   ./install-local.sh --no-start      只安装，不启动应用
#   ./install-local.sh --no-stop       安装前不强制停掉在跑的应用
#   ./install-local.sh --help          显示这段帮助
# ---------------------------------------------------------------------------
set -u

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
PROJECT_ROOT=$SCRIPT_DIR

APP_BUNDLE=com.hicodeer.studio
APP_ABILITY=EntryAbility
DEFAULT_HAP="$PROJECT_ROOT/hap/entry/build/default/outputs/default/entry-default-signed.hap"

SERVER_PORT=18710
DEV_PORT=""
HAP=""
DO_START=1
DO_STOP=1
DO_REINSTALL=0

# ---------------------------------------------------------------------------
# 输出helper
# ---------------------------------------------------------------------------
step()  { printf '\n[%s] %s\n' "$1" "$2"; }
ok()    { printf '      ✔ %s\n' "$1"; }
warn()  { printf '      ! %s\n' "$1" >&2; }
die()   { printf '\n[错误] %s\n' "$1" >&2; exit 1; }

usage() {
    # 打印文件头那段说明。用 awk 找到结尾的分隔线为止，这样以后增删注释行
    # 也不用同步改行号。
    awk 'NR < 3 { next } /^# ---/ { exit } { sub(/^# ?/, ""); print }' "$0"
}

# ---------------------------------------------------------------------------
# 参数
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        --hap)          HAP=${2:-}; shift 2 ;;
        --port)         DEV_PORT=${2:-}; shift 2 ;;
        --server-port)  SERVER_PORT=${2:-}; shift 2 ;;
        --no-start)     DO_START=0; shift ;;
        --no-stop)      DO_STOP=0; shift ;;
        --reinstall)    DO_REINSTALL=1; shift ;;
        -h|--help)      usage; exit 0 ;;
        *)              die "未知参数：$1（用 --help 看用法）" ;;
    esac
done

[ -n "$HAP" ] || HAP=$DEFAULT_HAP

# ---------------------------------------------------------------------------
# 定位 hdc
# ---------------------------------------------------------------------------
find_hdc() {
    if [ -n "${HDC:-}" ] && [ -x "$HDC" ]; then printf '%s' "$HDC"; return 0; fi
    if command -v hdc >/dev/null 2>&1; then command -v hdc; return 0; fi
    for c in "$HOME/.harmonybrew/bin/hdc" /usr/bin/hdc /bin/hdc; do
        if [ -x "$c" ]; then printf '%s' "$c"; return 0; fi
    done
    return 1
}

has_python() { command -v python3 >/dev/null 2>&1; }

# 探测本机 hdcd 监听的调试端口：先看显式值，再看设备参数，最后扫常见端口
detect_device_port() {
    if has_python; then
        python3 - "$DEV_PORT" <<'PYEOF'
import socket, sys, subprocess

candidates = []
explicit = sys.argv[1] if len(sys.argv) > 1 else ""
if explicit.isdigit():
    candidates.append(int(explicit))

try:
    raw = subprocess.run(["/bin/param", "get", "persist.hdc.port"],
                         capture_output=True, text=True, timeout=5).stdout.strip()
    if raw.isdigit():
        candidates.append(int(raw))
except Exception:
    pass

candidates += [45661, 37581, 45675, 5543, 18710, 8710, 5555]

seen = set()
for port in candidates:
    if port in seen or port <= 0:
        continue
    seen.add(port)
    sock = socket.socket()
    sock.settimeout(0.6)
    try:
        sock.connect(("127.0.0.1", port))
        print(port)
        sys.exit(0)
    except Exception:
        pass
    finally:
        sock.close()
sys.exit(1)
PYEOF
        return $?
    fi

    # 没有 python3 时只能靠设备参数
    if [ -n "$DEV_PORT" ]; then printf '%s' "$DEV_PORT"; return 0; fi
    p=$(/bin/param get persist.hdc.port 2>/dev/null | tr -d ' \r\n')
    case "$p" in
        ''|*[!0-9]*) return 1 ;;
        *)           printf '%s' "$p"; return 0 ;;
    esac
}

# 当前 env（OHOS_HDC_SERVER_PORT）下，hdc 服务端是否可用。
# 判据只看「客户端能否问到一个像样的回答」：[Empty] 或设备列表都算通，
# 空输出 / Failed / error 才算不通。
channel_alive() {
    out=$("$HDC" list targets 2>&1 | tr -d '\r')
    case "$out" in
        "")                                          return 1 ;;
        *[Ff]ail*|*[Ee]rror*|*"not found"*)          return 1 ;;
        *)                                           return 0 ;;
    esac
}

# 选一条可用的 hdc server 通道，成功时把 CHANNEL 设成给用户看的描述。
# 不预先 kill：复用已经在跑的 server 最稳，残留的重复 target 交给
# 第 3 步的 tconn ... -remove 处理。
select_channel() {
    # a) 指定端口。注意 OHOS_HDC_SERVER_PORT 只是「优先连这个 TCP server」的
    #    意愿：该端口没有 server 监听时，客户端会安静地回退到 UDS
    #    /data/hdc/hdc_debug/hdc_server，此时这里判为「通」，但物理通道是 UDS。
    export OHOS_HDC_SERVER_PORT=$SERVER_PORT
    if channel_alive; then
        CHANNEL="OHOS_HDC_SERVER_PORT=$SERVER_PORT（客户端可能已回退到 UDS）"
        return 0
    fi

    # b) 默认通道（不设 OHOS_HDC_SERVER_PORT）
    unset OHOS_HDC_SERVER_PORT
    if channel_alive; then
        CHANNEL="默认通道（未指定端口）"
        return 0
    fi

    # c) 都不通，最后尝试拉起一个。普通 shell 通常会失败在这里。
    "$HDC" kill >/dev/null 2>&1 || true
    export OHOS_HDC_SERVER_PORT=$SERVER_PORT
    if channel_alive; then
        CHANNEL="新拉起的 server（OHOS_HDC_SERVER_PORT=$SERVER_PORT）"
        return 0
    fi

    return 1
}

hdc_log_tail() {
    log=$(ls -t "$HOME/.hdc"/hdc-*.log 2>/dev/null | head -1)
    [ -n "$log" ] || return 0
    printf '      最近一次 hdc 日志：%s\n' "$log"
    grep -E 'SetUdsListen|bind uds|Initial:|Main finish' "$log" 2>/dev/null | tail -5 | sed 's/^/        /'
}

# ---------------------------------------------------------------------------
# 前置检查
# ---------------------------------------------------------------------------
HDC=$(find_hdc) || die "找不到 hdc。请把 hdc 放进 PATH，或用 HDC=/path/to/hdc 指定。"

[ -f "$HAP" ] || die "HAP 不存在：$HAP
     先跑 ./script/bundle-ohos --release（或 --debug）产出它。"

export OHOS_HDC_SERVER_PORT=$SERVER_PORT

printf '\n================ 本机直连安装 HiCodeer ================\n'
printf '  hdc          : %s\n' "$HDC"
printf '  HAP          : %s (%s 字节)\n' "$HAP" "$(stat -c %s "$HAP" 2>/dev/null || echo '?')"
printf '  hdc server 端口: %s\n' "$SERVER_PORT"
printf '  应用         : %s / %s\n' "$APP_BUNDLE" "$APP_ABILITY"

# ---------------------------------------------------------------------------
# 1. 探测设备调试端口
# ---------------------------------------------------------------------------
step "1/7" "探测本机 hdcd 调试端口"
DEV_PORT=$(detect_device_port) || {
    warn "没有找到任何在监听的回环端口"
    warn "常见原因：设备的「无线调试 / HDC 调试」没打开，或端口变了"
    warn "请打开开关后重试，或用 --port <端口> 手动指定"
    exit 1
}
ok "设备调试端口 = $DEV_PORT"

# ---------------------------------------------------------------------------
# 2. 拉起干净的 hdc server
# ---------------------------------------------------------------------------
step "2/7" "选择 hdc server 通道"
CHANNEL=""
if ! select_channel; then
    warn "三条通道都不通：指定端口 $SERVER_PORT、默认通道（UDS）、新拉起的 server"
    hdc_log_tail
    warn "若日志里是 'bind uds addr fail! ret:-13'，说明当前 shell（uid $(id -u)）"
    warn "无权 bind hdc 的 UDS /data/hdc/hdc_debug/hdc_server，且没有可复用的 server"
    warn "可试：① 换一个已经在跑 hdc server 的终端；② --server-port 指定别的端口"
    exit 1
fi
ok "server 通道 = $CHANNEL"

# ---------------------------------------------------------------------------
# 3. tconn 回环
# ---------------------------------------------------------------------------
step "3/7" "tconn 127.0.0.1:$DEV_PORT"
"$HDC" tconn "127.0.0.1:$DEV_PORT" -remove >/dev/null 2>&1 || true
if ! "$HDC" tconn "127.0.0.1:$DEV_PORT"; then
    warn "tconn 失败。若报 need connect-key，先执行："
    warn "  $( [ -n "${OHOS_HDC_SERVER_PORT:-}" ] && printf 'OHOS_HDC_SERVER_PORT=%s ' "$OHOS_HDC_SERVER_PORT" )$HDC tconn 127.0.0.1:$DEV_PORT -remove"
    exit 1
fi

# ---------------------------------------------------------------------------
# 4. 校验设备在线
# ---------------------------------------------------------------------------
step "4/7" "校验设备已登记"
TARGETS=$("$HDC" list targets 2>&1 | tr -d '\r')
printf '      list targets -> %s\n' "$TARGETS"
case "$TARGETS" in
    *"[Empty]"*|"") die "设备没连上。检查 --port 是否正确、调试开关是否打开。" ;;
esac
ok "设备在线"

# ---------------------------------------------------------------------------
# 5. 停旧应用 / 卸载重装
# ---------------------------------------------------------------------------
if [ "$DO_REINSTALL" = 1 ]; then
    step "5/7" "先卸载（让 hicodeerd 换新版）"
    "$HDC" uninstall "$APP_BUNDLE" 2>&1 | tail -2
    ok "已卸载（应用沙箱已清，数据根不受影响）"
else
    step "5/7" "停掉正在运行的旧应用"
    if [ "$DO_STOP" = 1 ]; then
        "$HDC" shell aa force-stop "$APP_BUNDLE" 2>&1 | tail -1 || true
        sleep 1
        ok "已停"
    else
        ok "跳过（--no-stop）"
    fi
fi

# ---------------------------------------------------------------------------
# 6. 安装
# ---------------------------------------------------------------------------
step "6/7" "安装 HAP"
INSTALL_OUT=$("$HDC" install -r "$HAP" 2>&1)
printf '%s\n' "$INSTALL_OUT" | tail -4 | sed 's/^/      /'
case "$INSTALL_OUT" in
    *successfully*|*success*) ok "安装成功" ;;
    *) die "安装失败（上面是 hdc 的原始输出）" ;;
esac

# ---------------------------------------------------------------------------
# 7. 启动
# ---------------------------------------------------------------------------
if [ "$DO_START" = 1 ]; then
    step "7/7" "启动应用"
    "$HDC" shell aa start -a "$APP_ABILITY" -b "$APP_BUNDLE" 2>&1 | tail -3 | sed 's/^/      /'
    ok "已下发启动命令"
else
    step "7/7" "跳过启动（--no-start）"
fi

printf '\n================ 完成 ================\n'
printf '注意：hicodeerd 由系统以独立 uid 拉起，不随 HAP 覆盖安装重启。\n'
printf '     磁盘上的 .hnp 换新后，要让它生效请二选一：\n'
printf '       a) 本脚本加 --reinstall（先卸载再装）\n'
printf '       b) 重启设备\n'
printf '\n'
