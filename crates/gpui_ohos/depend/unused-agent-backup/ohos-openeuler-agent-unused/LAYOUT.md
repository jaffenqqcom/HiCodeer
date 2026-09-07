# The cmd-agent crates (cmd-agent-protocol, cmd-agent, cmd-agentd)
# are NOT members of the zcoder top-level workspace (see Cargo.toml members).
# They are pulled in as path dependencies by util (cmd-agent /
# cmd-agent-protocol) and built standalone by script/bundle-ohos
# (cmd-agentd, host target). This file deliberately declares no
# [workspace]: doing so would make cargo resolve the crates twice (once per
# workspace), breaking type identity across the util/command boundary. Keep it
# as a placeholder that documents the layout.
