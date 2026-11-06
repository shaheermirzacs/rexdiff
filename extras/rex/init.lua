-- rexdiff: git changes panel next to the focused pane.
-- https://github.com/victorvf/rexdiff — expects the binary at ~/.local/bin/rexdiff (`make install`).

local terminal = "com.superlogical.terminal"

-- Folder of the focused terminal: the one the shell reported, or the running program's.
local function pane_dir(block_id)
  local r = rex.block.call(terminal, "pwd", { block_id = block_id })
  if r and r.pwd and r.pwd ~= "" then return r.pwd end
  local p = rex.block.call(terminal, "process", { block_id = block_id })
  return p and p.foreground and p.foreground.cwd or ""
end

rex.action{
  name = "rexdiff",
  title = "rexdiff: Toggle Changes",
  description = "Opens or closes the git changes panel next to the focused pane.",
  category = "Git",
  keywords = "git diff changes rexdiff",
  run = function(ctx)
    if not ctx.block_id then return end
    local r, err = rex.exec(
      { os.getenv("HOME") .. "/.local/bin/rexdiff", "toggle", "--focus", "--cwd", pane_dir(ctx.block_id) },
      { env = { REX_SESSION = ctx.session_id, REX_BLOCK = ctx.block_id }, timeout = 15 }
    )
    if not r then
      rex.log("error", "rexdiff: " .. tostring(err))
    elseif not r.ok then
      rex.log("warn", "rexdiff: " .. r.stderr)
    end
  end,
}

-- Option+Shift+? (Rex wants the key without Shift applied: "?" is Shift+/).
rex.bind("alt+shift+/", "rexdiff")
