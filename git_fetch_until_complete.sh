#!/usr/bin/env bash
#
# git_fetch_until_complete.sh
# 功能：重复执行 git fetch <远端>，直到拉取完整成功为止。
#
# 背景：网络不稳定时，单次 git fetch 可能中途断开（如 RPC failed、early EOF、
#       Connection reset by peer），只拉取部分引用。git fetch 会复用本地已下载
#       的对象，每次重试都从断点继续。本脚本循环重试，直到某次 fetch 返回成功。
#
# 输出：git 的 stdout（正确输出）与 stderr（错误输出）均原样直通终端，
#       一字不漏、实时显示，不做任何捕获或过滤。
# 重试：失败后立即重试，不设超时，不设等待。
#
# 用法：
#   ./git_fetch_until_complete.sh [远端名] [最大重试次数]
#   默认值：远端名=origin，最大重试次数=10。

set -u

# ---------- 常量配置 ----------
DEFAULT_REMOTE_NAME="origin"       # 默认远端名
DEFAULT_MAX_ATTEMPT_COUNT=10       # 最大重试次数，防止网络长期异常时无限循环
LOG_PREFIX="git_fetch_until_complete"

# ---------- 参数解析 ----------
REMOTE_NAME="${1:-$DEFAULT_REMOTE_NAME}"
MAX_ATTEMPT_COUNT="${2:-$DEFAULT_MAX_ATTEMPT_COUNT}"

# ---------- 日志函数 ----------
log_info() {
    echo "[$LOG_PREFIX][$(date '+%Y-%m-%d %H:%M:%S')][info] $*"
}

log_error() {
    echo "[$LOG_PREFIX][$(date '+%Y-%m-%d %H:%M:%S')][error] $*" >&2
}

# ---------- 主流程 ----------
# git fetch 不捕获、不重定向，stdout 与 stderr 均原样透传到终端；
# 完整性只以退出码为判据，git 一旦检测到传输中断必然非零退出。
attempt_count=1
while true; do
    log_info "开始第 $attempt_count 次 git fetch $REMOTE_NAME"
    if git fetch --depth=1 "$REMOTE_NAME"; then
        log_info "git fetch $REMOTE_NAME 成功，共尝试 $attempt_count 次"
        exit 0
    fi

    if [ "$attempt_count" -ge "$MAX_ATTEMPT_COUNT" ]; then
        log_error "已连续重试 $MAX_ATTEMPT_COUNT 次仍未完整 fetch，请检查网络连接后人工重试"
        exit 1
    fi

    attempt_count=$((attempt_count + 1))
done
