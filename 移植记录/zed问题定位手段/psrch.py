#!/usr/bin/env python3
"""zcoder 代码考古检索器 —— 替代在本仓会静默失效的 grep。

用法:
  python3 psrch.py <roots> <regex> [--depend] [--ext rs,toml] [--max 200]

  <roots>  逗号分隔的目录或文件，相对当前目录或绝对路径均可
  <regex>  Python 正则

为什么需要它（重要，别省这一步）:
  本仓 bash `grep -rn "<pat>" crates ...` 会静默失效 —— 同一个文件 grep 无命中，
  而 sed / Read 明明有内容（在 agent_servers/src/acp.rs、agent_ui/.../elicitation.rs
  多次复现）。Grep 工具（ripgrep）对 crates/gpui_ohos/depend/** 与
  crates/agent_servers/src/acp.rs 也返回空。
  ⇒ 凡"零命中 / 不存在"这类结论，必须用本脚本或 Read 复核，
    否则会得出反向的错误结论（本项目已因此误判过一次"URL 形态根本不存在"）。

Examples:
  python3 移植记录/zed问题定位手段/psrch.py crates/agent_servers "authUrl|_codebuddy"
  python3 移植记录/zed问题定位手段/psrch.py crates/gpui_ohos/depend/cmd-agent "forward_stdin"
  python3 移植记录/zed问题定位手段/psrch.py crates/agent_ui --ext rs,toml --max 300
"""

import argparse
import os
import re
import sys

SKIP_DIRS = {"target", "node_modules", ".git", "build", "oh_modules", ".hvigor"}
DEFAULT_EXT = "rs,toml,json,ets,ts,py,sh,md"


def main():
    ap = argparse.ArgumentParser(description="zcoder 代码考古检索器（grep 的可靠替代）")
    ap.add_argument("roots", help="逗号分隔的目录/文件")
    ap.add_argument("pattern", help="Python 正则")
    ap.add_argument("--depend", action="store_true", help="也搜 depend/ 子目录（默认跳过）")
    ap.add_argument("--ext", default=DEFAULT_EXT, help="文件后缀，逗号分隔（默认 %s）" % DEFAULT_EXT)
    ap.add_argument("--max", type=int, default=200, help="最多打印多少行（默认 200）")
    args = ap.parse_args()

    exts = tuple("." + e.strip().lstrip(".") for e in args.ext.split(",") if e.strip())
    pat = re.compile(args.pattern)

    skip = set(SKIP_DIRS)
    if not args.depend:
        skip.add("depend")

    hits = 0
    for root in args.roots.split(","):
        root = root.strip()
        if not root:
            continue
        targets = []
        if os.path.isfile(root):
            targets.append(root)
        elif os.path.isdir(root):
            for dp, dn, fn in os.walk(root):
                dn[:] = [d for d in dn if d not in skip]
                for f in fn:
                    if f.endswith(exts):
                        targets.append(os.path.join(dp, f))
        else:
            print("!! 路径不存在: %s" % root, file=sys.stderr)
            continue

        for p in targets:
            try:
                with open(p, encoding="utf-8", errors="replace") as fh:
                    for i, line in enumerate(fh, 1):
                        if pat.search(line):
                            print("%s:%d: %s" % (p, i, line.rstrip()[:200]))
                            hits += 1
                            if hits >= args.max:
                                print("... 已达 --max %d 上限，可能还有更多" % args.max)
                                print("== 命中 %d 行（截断） ==" % hits)
                                return
            except OSError:
                continue

    print("== 命中 %d 行 ==" % hits)


if __name__ == "__main__":
    main()
