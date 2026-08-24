# The cmd-agent crates (cmd-agent-protocol, cmd-agent-client, cmd-agent-server)
# are NOT members of the zcoder top-level workspace (see Cargo.toml members).
# They are pulled in as path dependencies by util (cmd-agent-client /
# cmd-agent-protocol) and built standalone by script/bundle-ohos
# (cmd-agent-server, host target). This file deliberately declares no
# [workspace]: doing so would make cargo resolve the crates twice (once per
# workspace), breaking type identity across the util/command boundary. Keep it
# as a placeholder that documents the layout.
